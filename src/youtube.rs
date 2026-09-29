use regex::Regex;
use std::sync::OnceLock;

fn re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?:https?://)?(?:www\.|m\.|music\.)?(?:youtube\.com/(?:watch\?[^\s]*?v=|shorts/|live/|embed/)|youtu\.be/)([A-Za-z0-9_-]{6,16})",
        )
        .unwrap()
    })
}

/// Вытаскивает первый YouTube-линк из текста и нормализует до watch?v=<id>.
pub fn extract_youtube_url(text: &str) -> Option<String> {
    let id = re().captures(text)?.get(1)?.as_str();
    Some(format!("https://www.youtube.com/watch?v={id}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_formats() {
        for url in [
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://youtu.be/dQw4w9WgXcQ",
            "https://youtube.com/shorts/dQw4w9WgXcQ?feature=share",
            "http://m.youtube.com/watch?app=x&v=dQw4w9WgXcQ&t=30",
            "смотри: youtu.be/dQw4w9WgXcQ плз",
        ] {
            assert_eq!(
                extract_youtube_url(url).as_deref(),
                Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ"),
                "failed for {url}"
            );
        }
    }

    #[test]
    fn non_youtube_is_ignored() {
        assert_eq!(extract_youtube_url("just chatting"), None);
        assert_eq!(extract_youtube_url("https://vimeo.com/12345"), None);
        assert_eq!(extract_youtube_url(""), None);
    }
}
