# 双跑测试配置注入：把「确定性回归规则集」写入 PBH 配置（Java profile.yml /
# Rust 单文件 config.yml，两侧 YAML 键名同构、缩进不同，用正则锚点处理）。
#
# 注入内容（两侧一致）：
# - ip-address-blocker.ips  += 203.0.114.0/24（CIDR）、198.51.100.200（单 IP）
# - ip-address-blocker.ports = 39999（替换占位 0）
# - ip-address-blocker.cities = 浙江省 温州市（GeoCN 写法，需 GeoCN 库）
# - client-name-blacklist 追加 REGEX ^EvilClient
#
# 幂等：重复注入不叠加。用法：python inject_test_profile.py <yml 路径>...
import re
import sys
import pathlib

IPS = ["203.0.114.0/24", "198.51.100.200"]
PORT = "39999"
CITY = "浙江省 温州市"
REGEX_RULE = '{"method":"REGEX","content":"^EvilClient.*"}'
# idle 防护的加速参数：速度阈值放大到「任意速度都算空闲」、空闲上限缩到 3s，
# 使对跑在 2~3 波内即可命中 idleTimeout（判定路径与默认配置完全一致）
IDLE_SPEED = "1000000000"
IDLE_MAX = "3000"


def inject_ips(text):
    """ips 占位（- 0.0.0.0 / - "0.0.0.0"）→ 测试 CIDR + 单 IP；保留原缩进。"""

    def repl(m):
        indent, quote = m.group(1), m.group(3) or ""
        items = "".join(f"{indent}- {quote}{ip}{quote}\n" for ip in IPS)
        return f"{indent}ips:\n{items}"

    # 已注入则跳过（幂等）
    if "203.0.114.0/24" in text:
        return text
    return re.sub(r"([ \t]+)ips:\r?\n[ \t]+-( )?(\"?)0\.0\.0\.0\3", repl, text, count=1)


def inject_ports(text):
    if "39999" in text:
        return text

    def repl(m):
        indent, quote = m.group(1), m.group(3) or ""
        return f"{indent}ports:\n{indent}- {quote}{PORT}{quote}"

    return re.sub(r"([ \t]+)ports:\r?\n[ \t]+-( )?(\"?)0\3?(?=\r?\n)", repl, text, count=1)


def inject_cities(text):
    """cities 占位（Java 默认衢州/温州两行、Rust 单文件占位「示例海南」）→ 仅温州。"""
    if "浙江省 温州市" in text:
        return text
    # Rust 形态：- "示例海南"
    text, n1 = re.subn(
        r'([ \t]+)- "示例海南"', lambda m: f"{m.group(1)}- \"{CITY}\"", text, count=1
    )
    if n1:
        return text
    # Java 形态：两行默认城市（衢州/温州）
    return re.sub(
        r"([ \t]+)- 浙江省 衢州市\r?\n[ \t]+- 浙江省 温州市",
        lambda m: f"{m.group(1)}- {CITY}",
        text,
        count=1,
    )


def inject_client_regex(text):
    """banned-client-name 列表头插入 REGEX 规则（缩进跟随该序列既有首项——
    Java 模板列表项与键同缩进、Rust 模板深 2 格，不能用统一偏移，
    否则同一序列缩进不一致会让 snakeyaml 拒载整个 profile）。"""
    if "EvilClient" in text:
        return text

    def repl(m):
        # 向后找该序列的第一个列表项缩进；找不到则退回键缩进
        rest = text[m.end():]
        item = re.search(r"^[ \t]+- ", rest, re.MULTILINE)
        indent = item.group(0)[:-2] if item else m.group(1) + "  "
        return f"{m.group(0)}\n{indent}- '{REGEX_RULE}'"

    return re.sub(r"([ \t]+)banned-client-name:(?=\r?\n)", repl, text, count=1)


def inject_idle(text):
    """idle-connection-dos-protection 启用 + 加速：速度阈值 64 → 1e9、
    max-allowed-idle-time 300000 → 3000（上游默认 enabled: false，须一并启用）。"""
    if "1000000000" in text:
        return text
    text = re.sub(
        r"([ \t]+idle-connection-dos-protection:\r?\n[ \t]+enabled: )false",
        lambda m: f"{m.group(1)}true",
        text,
        count=1,
    )
    text = re.sub(
        r"([ \t]+idle-speed-threshold: )\d+",
        lambda m: f"{m.group(1)}{IDLE_SPEED}",
        text,
        count=1,
    )
    text = re.sub(
        r"([ \t]+max-allowed-idle-time: )\d+",
        lambda m: f"{m.group(1)}{IDLE_MAX}",
        text,
        count=1,
    )
    return text


def main():
    changed = 0
    for arg in sys.argv[1:]:
        path = pathlib.Path(arg)
        text = path.read_text(encoding="utf-8")
        new = inject_ips(text)
        new = inject_ports(new)
        new = inject_cities(new)
        new = inject_client_regex(new)
        new = inject_idle(new)
        if new != text:
            path.write_text(new, encoding="utf-8")
            changed += 1
            print(f"已注入: {path}")
        else:
            print(f"无变化（可能已注入）: {path}")
    sys.exit(0 if changed else 0)


if __name__ == "__main__":
    main()
