use reqwest_middleware::ClientWithMiddleware;

pub struct PublicLinkSessionMiddleware {
    pub session_uid: String,
    pub access_token: String,
}

#[async_trait::async_trait]
impl reqwest_middleware::Middleware for PublicLinkSessionMiddleware {
    async fn handle(
        &self,
        mut request: reqwest::Request,
        extensions: &mut http::Extensions,
        next: reqwest_middleware::Next<'_>,
    ) -> reqwest_middleware::Result<reqwest::Response> {
        request.headers_mut().insert(
            "x-pm-uid",
            self.session_uid
                .parse()
                .map_err(reqwest_middleware::Error::middleware)?,
        );
        request.headers_mut().insert(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", self.access_token)
                .parse()
                .map_err(reqwest_middleware::Error::middleware)?,
        );

        if let Some(rewritten) = public_link_path(request.url().path()) {
            request.url_mut().set_path(&rewritten);
        }
        next.run(request, extensions).await
    }
}

fn public_link_path(path: &str) -> Option<String> {
    (path.starts_with("/drive/")
        && !path.starts_with("/drive/urls/")
        && !path.starts_with("/drive/v2/urls/")
        && !path.starts_with("/drive/unauth/"))
    .then(|| path.replacen("/drive/", "/drive/unauth/", 1))
}

pub fn create_client_with_timeout(timeout_seconds: f64) -> ClientWithMiddleware {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs_f64(timeout_seconds))
        .build()
        .expect("failed to build HTTP client");

    reqwest_middleware::ClientBuilder::new(client).build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaves_public_link_session_routes_unchanged() {
        for path in [
            "/drive/urls/anything",
            "/drive/urls/drive/anything",
            "/drive/urls/drive/v2/anything",
            "/drive/v2/urls/anything",
            "/drive/v2/urls/drive/anything",
            "/drive/v2/urls/drive/v2/anything",
        ] {
            assert_eq!(public_link_path(path), None);
        }
    }

    #[test]
    fn prefixes_v2_drive_routes() {
        assert_eq!(
            public_link_path("/drive/v2/anything").as_deref(),
            Some("/drive/unauth/v2/anything")
        );
        assert_eq!(
            public_link_path("/drive/v2/drive/anything").as_deref(),
            Some("/drive/unauth/v2/drive/anything")
        );
        assert_eq!(
            public_link_path("/drive/v2/drive/v2/anything").as_deref(),
            Some("/drive/unauth/v2/drive/v2/anything")
        );
    }

    #[test]
    fn prefixes_non_v2_drive_routes() {
        assert_eq!(
            public_link_path("/drive/anything").as_deref(),
            Some("/drive/unauth/anything")
        );
        assert_eq!(
            public_link_path("/drive/anything/v2/anything").as_deref(),
            Some("/drive/unauth/anything/v2/anything")
        );
        assert_eq!(
            public_link_path("/drive/anything/drive/anything").as_deref(),
            Some("/drive/unauth/anything/drive/anything")
        );
    }

    #[test]
    fn leaves_non_drive_routes_unchanged() {
        assert_eq!(public_link_path("/storage/blob"), None);
        assert_eq!(public_link_path("/core/v4/events"), None);
    }
}
