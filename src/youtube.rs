use regex::Regex;
use reqwest::Url;
use std::sync::OnceLock;

fn re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Tokenize complete URLs before validating their host. Consuming an
        // unrelated URL also prevents extracting a nested URL from its query.
        Regex::new(
            r#"(?i)(?:^|[\s(<{\[«\"'`:,;])(?P<url>(?:[a-z][a-z0-9+.-]*://|(?:www\.|m\.|music\.)?(?:youtube\.com|youtu\.be)/)[^\s<>(){}\[\]«»\"'`]+)"#,
        )
        .expect("valid URL candidate expression")
    })
}

/// Extract the first valid YouTube URL from surrounding text.
pub fn extract_youtube_url(text: &str) -> Option<String> {
    re().captures_iter(text).find_map(|captures| {
        let candidate = captures.name("url")?.as_str();
        normalize_youtube_url(candidate.trim_end_matches(['.', ',', '!', '?', ';', ':']))
    })
}

/// Validate a single URL and return its canonical watch URL.
pub fn normalize_youtube_url(candidate: &str) -> Option<String> {
    let candidate = candidate.trim();
    if candidate.starts_with('/')
        || candidate.contains('\\')
        || candidate.chars().any(char::is_whitespace)
    {
        return None;
    }
    let authority = candidate
        .split_once("://")
        .map_or(candidate, |(_, rest)| rest)
        .split(['/', '?', '#'])
        .next()?;
    if authority.contains('@') {
        return None;
    }
    let url = if candidate.contains("://") {
        Url::parse(candidate)
    } else {
        Url::parse(&format!("https://{candidate}"))
    }
    .ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return None;
    }
    let short = match url.host_str()? {
        "youtu.be" | "www.youtu.be" | "m.youtu.be" | "music.youtu.be" => true,
        "youtube.com" | "www.youtube.com" | "m.youtube.com" | "music.youtube.com" => false,
        _ => return None,
    };
    let path = url.path().strip_suffix('/').unwrap_or(url.path());
    let id = if short {
        path.strip_prefix('/')?.to_string()
    } else if path == "/watch" {
        let mut ids = url.query_pairs().filter(|(key, _)| key == "v");
        let (_, id) = ids.next()?;
        if ids.next().is_some() {
            return None;
        }
        id.into_owned()
    } else {
        let (kind, id) = path.strip_prefix('/')?.split_once('/')?;
        if !matches!(kind, "shorts" | "live" | "embed") {
            return None;
        }
        id.to_string()
    };
    if id.len() != 11
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return None;
    }
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
            "https://music.youtube.com/watch?v=dQw4w9WgXcQ&list=playlist",
            "https://youtube.com/live/dQw4w9WgXcQ#t=30",
            "https://youtube.com/embed/dQw4w9WgXcQ/",
            "https://www.youtu.be/dQw4w9WgXcQ?si=share",
            "HTTPS://WWW.YOUTUBE.COM/watch?v=dQw4w9WgXcQ",
            "https://youtube.com:443/watch?v=dQw4w9WgXcQ",
            "http://youtube.com:80/watch?v=dQw4w9WgXcQ",
        ] {
            assert_eq!(
                extract_youtube_url(url).as_deref(),
                Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ"),
                "failed for {url}"
            );
        }
    }

    #[test]
    fn text_punctuation_and_multiple_candidates_are_supported() {
        for text in [
            "Смотри:https://youtu.be/dQw4w9WgXcQ!",
            "[ссылка](https://youtu.be/dQw4w9WgXcQ).",
            "«https://youtube.com/watch?v=dQw4w9WgXcQ», полезно.",
            "https://notyoutube.com/watch?v=invalidxxxx then youtu.be/dQw4w9WgXcQ",
            "https://youtu.be/invalid then https://youtu.be/dQw4w9WgXcQ",
        ] {
            assert_eq!(
                extract_youtube_url(text).as_deref(),
                Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ"),
                "failed for {text}",
            );
        }
    }

    #[test]
    fn rejects_unrelated_hosts_credentials_ports_and_nested_urls() {
        for text in [
            "just chatting",
            "",
            "https://vimeo.com/12345",
            "https://notyoutube.com/watch?v=dQw4w9WgXcQ",
            "notyoutube.com/watch?v=dQw4w9WgXcQ",
            "https://youtube.com.example.com/watch?v=dQw4w9WgXcQ",
            "https://example.youtube.com/watch?v=dQw4w9WgXcQ",
            "example.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://youtube.com./watch?v=dQw4w9WgXcQ",
            "https://user:password@youtube.com/watch?v=dQw4w9WgXcQ",
            "https://user@youtu.be/dQw4w9WgXcQ",
            "https://@youtube.com/watch?v=dQw4w9WgXcQ",
            "https://youtube.com:8443/watch?v=dQw4w9WgXcQ",
            "https://youtube.com:invalid/watch?v=dQw4w9WgXcQ",
            "ftp://youtube.com/watch?v=dQw4w9WgXcQ",
            "https://example.com/?next=https://youtube.com/watch?v=dQw4w9WgXcQ",
        ] {
            assert_eq!(extract_youtube_url(text), None, "accepted {text}");
        }
    }

    #[test]
    fn rejects_invalid_ids_paths_and_ambiguous_video_parameters() {
        for url in [
            "https://youtu.be/abcdef",
            "https://youtu.be/dQw4w9WgXcQextra",
            "https://youtu.be/dQw4w9WgXcQ/extra",
            "https://youtu.be/dQw4w9WgXcж",
            "https://youtube.com/watch?v=dQw4w9WgXcQextra",
            "https://youtube.com/watch?v=dQw4w9WgXc%21",
            "https://youtube.com/watch?v=dQw4w9WgXcQ&v=abcdefghijk",
            "https://youtube.com/watch?other=v=dQw4w9WgXcQ",
            "https://youtube.com/shorts/dQw4w9WgXcQ/extra",
            "https://youtube.com/playlist?list=dQw4w9WgXcQ",
        ] {
            assert_eq!(extract_youtube_url(url), None, "accepted {url}");
        }
        assert_eq!(
            normalize_youtube_url("https://youtu.be/aB_cD-eF012").as_deref(),
            Some("https://www.youtube.com/watch?v=aB_cD-eF012"),
        );
    }

    #[test]
    fn single_url_parser_rejects_surrounding_text() {
        for input in [
            "смотри: https://youtu.be/dQw4w9WgXcQ",
            "https://youtu.be/dQw4w9WgXcQ ещё текст",
            "//youtube.com/watch?v=dQw4w9WgXcQ",
            "https:youtube.com/watch?v=dQw4w9WgXcQ",
        ] {
            assert_eq!(normalize_youtube_url(input), None, "accepted {input}");
        }
    }
}
