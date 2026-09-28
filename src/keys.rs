use windows::Win32::UI::Input::KeyboardAndMouse::{
    VK_0, VK_A, VK_ADD, VK_BACK, VK_CANCEL, VK_CAPITAL, VK_CONTROL, VK_DECIMAL, VK_DELETE, VK_DIVIDE,
    VK_DOWN, VK_END, VK_ESCAPE, VK_F, VK_HOME, VK_INSERT, VK_LEFT, VK_LWIN, VK_MENU, VK_MULTIPLY,
    VK_NEXT, VK_NUMLOCK, VK_NUMPAD0, VK_OEM_1, VK_OEM_2, VK_OEM_3, VK_OEM_4, VK_OEM_5, VK_OEM_6,
    VK_OEM_7, VK_OEM_COMMA, VK_OEM_MINUS, VK_OEM_PERIOD, VK_OEM_PLUS, VK_PRIOR, VK_RCONTROL, VK_RETURN,
    VK_RIGHT, VK_RMENU, VK_RWIN, VK_SHIFT, VK_SNAPSHOT, VK_SPACE, VK_SUBTRACT, VK_TAB, VK_UP,
    VIRTUAL_KEY,
};

/// 把 "ctrl+left" 这类组合键描述解析成 VK 码序列（按下顺序，抬起按逆序）。
///
/// 出错时返回可读原因，直接显示给用户。
pub fn parse_combo(spec: &str) -> Result<Vec<u16>, String> {
    let mut keys = Vec::new();
    for token in spec.split('+') {
        let token = token.trim();
        if token.is_empty() {
            return Err(format!("组合键 {spec:?} 中有多余的 +"));
        }
        keys.push(parse_key(token).ok_or_else(|| format!("无法识别的按键 {token:?}"))?);
    }
    if keys.is_empty() {
        return Err("组合键为空".to_owned());
    }
    Ok(keys)
}

/// 把空格分隔的多个组合键拆成序列，例如 "Left Left Space"。
pub fn parse_sequence(input: &str) -> Result<Vec<Vec<u16>>, String> {
    input
        .split_whitespace()
        .map(parse_combo)
        .collect()
}

fn parse_key(token: &str) -> Option<u16> {
    let name = token.to_ascii_lowercase();
    let vk: VIRTUAL_KEY = match name.as_str() {
        "ctrl" | "control" => VK_CONTROL,
        "alt" | "menu" => VK_MENU,
        "shift" => VK_SHIFT,
        "win" | "meta" | "super" => VK_LWIN,
        "left" | "←" => VK_LEFT,
        "right" | "→" => VK_RIGHT,
        "up" | "↑" => VK_UP,
        "down" | "↓" => VK_DOWN,
        "enter" | "return" => VK_RETURN,
        "esc" | "escape" => VK_ESCAPE,
        "tab" => VK_TAB,
        "space" | "空格" => VK_SPACE,
        "backspace" | "back" => VK_BACK,
        "delete" | "del" => VK_DELETE,
        "insert" | "ins" => VK_INSERT,
        "home" => VK_HOME,
        "end" => VK_END,
        "pageup" | "pgup" => VK_PRIOR,
        "pagedown" | "pgdn" => VK_NEXT,
        "capslock" | "caps" => VK_CAPITAL,
        "printscreen" | "prtsc" => VK_SNAPSHOT,
        "-" | "subtract" => VK_SUBTRACT,
        "+" | "add" => VK_ADD,
        "*" | "multiply" => VK_MULTIPLY,
        "/" | "divide" => VK_DIVIDE,
        "." | "decimal" => VK_DECIMAL,
        other => return named_or_literal(other),
    };
    Some(vk.0)
}

fn named_or_literal(name: &str) -> Option<u16> {
    if let Some(digits) = name.strip_prefix('f')
        && let Ok(n) = digits.parse::<u8>()
        && (1..=24).contains(&n)
    {
        return Some(VK_F.0 + u16::from(n) - 1);
    }

    if let Some(digits) = name.strip_prefix("numpad")
        && let Ok(n) = digits.parse::<u8>()
        && n < 10
    {
        return Some(VK_NUMPAD0.0 + u16::from(n));
    }

    let mut chars = name.chars();
    let ch = chars.next()?;
    if chars.next().is_none() {
        return match ch {
            'a'..='z' => Some(VK_A.0 + (ch as u32 - 'a' as u32) as u16),
            '0'..='9' => Some(VK_0.0 + (ch as u32 - '0' as u32) as u16),
            ';' => Some(VK_OEM_1.0),
            '=' => Some(VK_OEM_PLUS.0),
            ',' => Some(VK_OEM_COMMA.0),
            '-' => Some(VK_OEM_MINUS.0),
            '.' => Some(VK_OEM_PERIOD.0),
            '/' => Some(VK_OEM_2.0),
            '`' => Some(VK_OEM_3.0),
            '[' => Some(VK_OEM_4.0),
            '\\' => Some(VK_OEM_5.0),
            ']' => Some(VK_OEM_6.0),
            '\'' => Some(VK_OEM_7.0),
            _ => None,
        };
    }
    None
}

/// SendInput 需要显式给出扩展键标志，靠查表比从 scan code 反推更可靠。
pub fn is_extended(vk: u16) -> bool {
    matches!(
        VIRTUAL_KEY(vk),
        VK_RIGHT
            | VK_LEFT
            | VK_DOWN
            | VK_UP
            | VK_HOME
            | VK_END
            | VK_PRIOR
            | VK_NEXT
            | VK_INSERT
            | VK_DELETE
            | VK_LWIN
            | VK_RWIN
            | VK_SNAPSHOT
            | VK_DIVIDE
            | VK_RCONTROL
            | VK_RMENU
            | VK_NUMLOCK
            | VK_CANCEL
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 单个按键与方向键() {
        assert_eq!(parse_combo("Left").unwrap(), vec![VK_LEFT.0]);
        assert_eq!(parse_combo("space").unwrap(), vec![VK_SPACE.0]);
        assert_eq!(parse_combo("F5").unwrap(), vec![VK_F.0 + 4]);
    }

    #[test]
    fn 字母数字按ascii对齐() {
        assert_eq!(parse_combo("w").unwrap(), vec![VK_A.0 + 22]);
        assert_eq!(parse_combo("7").unwrap(), vec![VK_0.0 + 7]);
    }

    #[test]
    fn 组合键保持顺序() {
        assert_eq!(
            parse_combo("Ctrl+Shift+Left").unwrap(),
            vec![VK_CONTROL.0, VK_SHIFT.0, VK_LEFT.0]
        );
    }

    #[test]
    fn 序列按空格拆分() {
        let seq = parse_sequence("Left Left Space").unwrap();
        assert_eq!(seq.len(), 3);
        assert_eq!(seq[2], vec![VK_SPACE.0]);
    }

    #[test]
    fn 未知按键给出可读错误() {
        assert!(parse_combo("Hyper+X").is_err());
        assert!(parse_combo("Ctrl+").is_err());
        assert!(parse_combo("").is_err());
    }
}
