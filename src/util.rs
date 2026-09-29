/// Обрезка по символам (не байтам) с многоточием — для ошибок и гигантских транскриптов.
pub fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_short() {
        assert_eq!(truncate("ок", 5), "ок");
    }

    #[test]
    fn cuts_by_chars_not_bytes() {
        let got = truncate("жжжжж", 3);
        assert_eq!(got, "жжж…");
    }
}
