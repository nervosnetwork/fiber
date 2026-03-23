use url::Url;

pub(crate) fn check_proxy_url(proxy_url: &str) -> Result<(), String> {
    let parsed_url = Url::parse(proxy_url).map_err(|e| e.to_string())?;
    if parsed_url.host_str().is_none() {
        return Err(format!("missing host in proxy url: {}", proxy_url));
    }
    let scheme = parsed_url.scheme();
    if scheme != "socks5" {
        return Err(format!(
            "fiber doesn't support proxy scheme: {}, only socks5 is supported",
            scheme
        ));
    }
    if parsed_url.port().is_none() {
        return Err(format!("missing port in proxy url: {}", proxy_url));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_socks5_url() {
        assert!(check_proxy_url("socks5://127.0.0.1:1080").is_ok());
        assert!(check_proxy_url("socks5://username:password@localhost:1080").is_ok());
    }

    #[test]
    fn test_invalid_scheme() {
        let result = check_proxy_url("http://127.0.0.1:1080");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("doesn't support proxy scheme"));
    }

    #[test]
    fn test_missing_port() {
        let result = check_proxy_url("socks5://127.0.0.1");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("missing port"));
    }

    #[test]
    fn test_invalid_url() {
        let result = check_proxy_url("not-a-url");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_socks5_url_components() {
        let parsed = Url::parse("socks5://username:password@localhost:1080").unwrap();
        assert_eq!(parsed.scheme(), "socks5");
        assert_eq!(parsed.username(), "username");
        assert_eq!(parsed.password(), Some("password"));
        assert_eq!(parsed.host_str(), Some("localhost"));
        assert_eq!(parsed.port(), Some(1080));
    }
}
