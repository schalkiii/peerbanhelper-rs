//! SMTP 邮件推送（lettre）。

use super::*;
use lettre::message::header::ContentType;
use lettre::message::{Mailbox, Message, SinglePart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::transport::smtp::SmtpTransport;
use lettre::Transport as _;
use std::str::FromStr;
use tracing::{error, warn};

// ---------------------------------------------------------------------------
// smtp
// ---------------------------------------------------------------------------

/// 对齐 `SmtpPushProvider.Config`。
#[derive(Clone, Debug)]

pub struct SmtpSettings {
    pub host: String,
    pub port: u16,
    pub auth: bool,
    pub username: String,
    pub password: String,
    pub sender: String,
    pub sender_name: String,
    pub receivers: Vec<String>,
    pub encryption: String,
    /// 对齐 `mail.smtp.sendpartial`：仅持久化、不参与发送（lettre 无等价开关，
    /// JavaMail 也只在该属性下处理「多收件人部分失败」，见 [`smtp_send_mail`] 注释）
    #[allow(dead_code)]
    pub send_partial: bool,
}

/// 对齐 `SmtpPushProvider.Encryption`（`Encryption.valueOf` 大小写敏感）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SmtpEncryption {
    None,
    StartTls,
    EnforceStartTls,
    SslTls,
}

/// 对齐 `Encryption.valueOf(config.getEncryption())`：仅精确匹配枚举名，其余返回 `None`
/// （上游此时 `log.error("Unable to load mail encryption type: ...")` 并**不启用任何 TLS**）。
pub(crate) fn parse_smtp_encryption(value: &str) -> Option<SmtpEncryption> {
    match value {
        "NONE" => Some(SmtpEncryption::None),
        "STARTTLS" => Some(SmtpEncryption::StartTls),
        "ENFORCE_STARTTLS" => Some(SmtpEncryption::EnforceStartTls),
        "SSLTLS" => Some(SmtpEncryption::SslTls),
        _ => None,
    }
}

/// 对齐 `SmtpPushProvider.push`：发送失败记录 warn 并返回 `false`（不向管理器抛出）。
///
/// 与上游的两点差异（不影响 `PushManager` 的 OR 结果）：
/// 1. 上游只捕获 `MessagingException`，其它异常会冒泡给 `PushManagerImpl`；
///    lettre 的错误类型无法区分，本移植把所有发送失败都规约为 `Ok(false)` + warn；
/// 2. 上游在告警线程内同步阻塞发送；本移植用 `spawn_blocking` 承载阻塞式 SMTP，
///    对调用方仍表现为「await 完成一次发送」。
pub(crate) async fn smtp_push(settings: &SmtpSettings, title: &str, content: &str) -> anyhow::Result<bool> {
    let settings = settings.clone();
    let title = title.to_string();
    let content = content.to_string();
    let result =
        tokio::task::spawn_blocking(move || smtp_send_mail(&settings, &title, &content)).await;
    match result {
        Ok(Ok(())) => Ok(true),
        Ok(Err(e)) => {
            warn!("Unable to push message via SMTP: {e}");
            Ok(false)
        }
        Err(e) => {
            warn!("Unable to push message via SMTP: {e}");
            Ok(false)
        }
    }
}

/// 对齐 `SmtpPushProvider.sendMail`：每次发送都新建 Session/Transport，设置发件人
/// （显示名取配置的 `name`）、收件人（非法地址 warn 后跳过）、主题（UTF-8）与
/// `text/html; charset=UTF-8` 正文（正文是 markdown 渲染后的 HTML）。
///
/// 与上游的差异：
/// - 上游返回 JavaMail 的 `MimeMessage.getMessageID()`；lettre 的 `Response` 不含 Message-ID，
///   本移植返回 `()`（调用方本就不使用该值）；
/// - `mail.smtp.sendpartial` 无 lettre 等价开关（JavaMail 仅在多收件人部分失败时生效）。
fn smtp_send_mail(settings: &SmtpSettings, subject: &str, text: &str) -> anyhow::Result<()> {
    // 对齐上游 `mail.smtp.*` 属性：host/port 恒取配置值（`getInt("port")` 缺省为 0）
    let mut builder = SmtpTransport::builder_dangerous(settings.host.as_str()).port(settings.port);
    match parse_smtp_encryption(&settings.encryption) {
        Some(SmtpEncryption::None) => {}
        Some(SmtpEncryption::StartTls) => {
            builder = builder.tls(Tls::Opportunistic(smtp_tls_parameters(&settings.host)?));
        }
        Some(SmtpEncryption::EnforceStartTls) => {
            builder = builder.tls(Tls::Required(smtp_tls_parameters(&settings.host)?));
        }
        Some(SmtpEncryption::SslTls) => {
            builder = builder.tls(Tls::Wrapper(smtp_tls_parameters(&settings.host)?));
        }
        None => {
            // 对齐上游 `log.error("Unable to load mail encryption type: {}, it's valid?")`
            // 之后继续发送（不启用任何 TLS）
            error!(
                "Unable to load mail encryption type: {}, it's valid?",
                settings.encryption
            );
        }
    }
    if settings.auth {
        builder = builder.credentials(Credentials::new(
            settings.username.clone(),
            settings.password.clone(),
        ));
    }
    let transport = builder.build();

    let message = smtp_build_message(settings, subject, text)?;
    transport
        .send(&message)
        .map_err(|e| anyhow::anyhow!("unable to send mail: {e}"))?;
    Ok(())
}

/// 组装邮件（对齐 `SmtpPushProvider.sendMail` 中 MimeMessage 部分的四步：
/// 发件人、收件人、主题、HTML 正文）。
pub(crate) fn smtp_build_message(
    settings: &SmtpSettings,
    subject: &str,
    text: &str,
) -> anyhow::Result<Message> {
    let mut from = Mailbox::from_str(&settings.sender)
        .map_err(|e| anyhow::anyhow!("invalid sender address [{}]: {e}", settings.sender))?;
    // 对齐 `new InternetAddress(sender, senderName, "UTF-8")`：显示名取配置的 `name`
    from.name = Some(settings.sender_name.clone());

    let mut recipients = Vec::new();
    for raw in &settings.receivers {
        // 对齐上游逐个解析、非法地址 warn 后跳过
        match Mailbox::from_str(raw) {
            Ok(mailbox) => recipients.push(mailbox),
            Err(e) => warn!("The email address [{raw}] is invalid: {e}"),
        }
    }
    if recipients.is_empty() {
        // 对齐 JavaMail 在无有效收件人时抛出的 SendFailedException("No recipient addresses")
        anyhow::bail!("No recipient addresses");
    }

    let mut message_builder = Message::builder().from(from);
    for recipient in recipients {
        message_builder = message_builder.to(recipient);
    }
    message_builder
        .subject(subject)
        .singlepart(
            SinglePart::builder()
                // 上游 `setContent(..., "text/html; charset=UTF-8")`：`TEXT_HTML` 只有
                // `text/html`，缺 charset 时中文正文会被客户端按其它编码解出乱码。
                .header(
                    ContentType::parse("text/html; charset=utf-8")
                        .unwrap_or(ContentType::TEXT_HTML),
                )
                .body(markdown_to_html(text)),
        )
        .map_err(|e| anyhow::anyhow!("unable to build mail message: {e}"))
}

fn smtp_tls_parameters(host: &str) -> anyhow::Result<TlsParameters> {
    TlsParameters::new(host.to_string()).map_err(|e| anyhow::anyhow!("invalid TLS parameters: {e}"))
}
