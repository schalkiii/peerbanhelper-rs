//! `/api/egg`：上游 `PBHEasterEggController` 的隐藏彩蛋，随机 302 到一条精选链接。
//!
//! 上游不注册任何前端入口，也没有 Role 限制（浏览器直接访问 `/api/egg` 触发）；
//! 链接列表与顺序照抄上游，避免"随机到不同歌单/视频"的偏差。

use axum::{
    http::{header::LOCATION, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use rand::Rng;

const URLS: [&str; 22] = [
    "https://www.bilibili.com/video/BV1S3UaYvE5o",
    "https://www.bilibili.com/video/BV1QQ4y1d7Rd",
    "https://www.bilibili.com/video/BV1by4y1F7nS",
    "https://www.bilibili.com/video/BV1ztzBY1EPy",
    "https://www.bilibili.com/video/BV15Z4y1Q7mz",
    "https://www.bilibili.com/video/BV1GJ411x7h7",
    "https://www.bilibili.com/video/BV1a14y187U5",
    "https://www.bilibili.com/video/BV1na411Q7qH",
    "https://music.163.com/song?id=2647873613&uct2=U2FsdGVkX1+Uch9IV1F1N/gLpHilPuUZQXy0xDGsKrM=",
    "https://music.163.com/song?id=2165896793&uct2=U2FsdGVkX19w55MRVVy7ZG3Yf9d6QL+CIwgE8jPc4N0=",
    "https://www.youtube.com/watch?v=tSLI6PjAfns",
    "https://www.bilibili.com/video/BV1Nb41117L3",
    "https://www.bilibili.com/video/BV1js411A71E",
    "https://store.steampowered.com/app/365450/Hacknet/",
    "https://store.steampowered.com/app/753640/Outer_Wilds/",
    "https://store.steampowered.com/app/739630/Phasmophobia/",
    "https://store.steampowered.com/app/3590/Plants_vs_Zombies_GOTY_Edition/",
    "https://store.steampowered.com/app/333600/NEKOPARA_Vol_1/",
    "https://store.steampowered.com/app/391540/Undertale/",
    "https://store.steampowered.com/app/646570/Slay_the_Spire/",
    "https://store.steampowered.com/app/457140/_/",
    "https://www.bilibili.com/video/BV1nB3u6tEHD",
];

/// 对齐 Javalin `context.redirect(url)` 的 302 Found（而 axum `Redirect` 是 303/307）。
pub async fn egg() -> Response {
    let index = rand::thread_rng().gen_range(0..URLS.len());
    let mut response = StatusCode::FOUND.into_response();
    response
        .headers_mut()
        .insert(LOCATION, HeaderValue::from_static(URLS[index]));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_list_matches_upstream() {
        assert_eq!(URLS.len(), 22);
        assert!(URLS.iter().all(|url| url.starts_with("https://")));
    }
}
