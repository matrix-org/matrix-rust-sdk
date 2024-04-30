//! Helper traits to convert between http 0.2 and http 1.x.

/// Convert from http 1.x to http 0.2
pub trait ToHttpOld {
    type Output;

    fn to_http_old(self) -> Self::Output;
}

impl<T> ToHttpOld for http::Response<T> {
    type Output = http_old::Response<T>;

    fn to_http_old(self) -> Self::Output {
        let (parts, body) = self.into_parts();
        let mut response = http_old::Response::new(body);

        *response.status_mut() = parts.status.to_http_old();
        *response.version_mut() = parts.version.to_http_old();
        *response.headers_mut() = parts.headers.to_http_old();
        // We cannot do anything about extensions because we cannot iterate over them.

        response
    }
}

impl ToHttpOld for http::StatusCode {
    type Output = http_old::StatusCode;

    fn to_http_old(self) -> Self::Output {
        http_old::StatusCode::from_u16(self.as_u16())
            .expect("status code should be valid between both versions")
    }
}

impl ToHttpOld for http::Version {
    type Output = http_old::Version;

    fn to_http_old(self) -> Self::Output {
        if self == http::Version::HTTP_09 {
            http_old::Version::HTTP_09
        } else if self == http::Version::HTTP_10 {
            http_old::Version::HTTP_10
        } else if self == http::Version::HTTP_11 {
            http_old::Version::HTTP_11
        } else if self == http::Version::HTTP_2 {
            http_old::Version::HTTP_2
        } else if self == http::Version::HTTP_3 {
            http_old::Version::HTTP_3
        } else {
            // Current http code doesn't have other variants.
            unreachable!()
        }
    }
}

impl ToHttpOld for http::HeaderMap<http::HeaderValue> {
    type Output = http_old::HeaderMap<http_old::HeaderValue>;

    fn to_http_old(self) -> Self::Output {
        let mut map = http_old::HeaderMap::new();
        map.extend(
            self.into_iter()
                .map(|(name, value)| (name.map(ToHttpOld::to_http_old), value.to_http_old())),
        );

        map
    }
}

impl ToHttpOld for http::HeaderName {
    type Output = http_old::HeaderName;

    fn to_http_old(self) -> Self::Output {
        http_old::HeaderName::from_bytes(self.as_ref())
            .expect("header name should be valid between both versions")
    }
}

impl ToHttpOld for http::HeaderValue {
    type Output = http_old::HeaderValue;

    fn to_http_old(self) -> Self::Output {
        http_old::HeaderValue::from_bytes(self.as_bytes())
            .expect("header value should be valid between both versions")
    }
}

/// Convert from http 0.2 to http 1.x
pub trait ToHttpNew {
    type Output;

    fn to_http_new(self) -> Self::Output;
}

impl<T> ToHttpNew for http_old::Request<T> {
    type Output = http::Request<T>;

    fn to_http_new(self) -> Self::Output {
        let (parts, body) = self.into_parts();
        let mut request = http::Request::new(body);

        *request.method_mut() = parts.method.to_http_new();
        *request.uri_mut() = parts.uri.to_http_new();
        *request.version_mut() = parts.version.to_http_new();
        *request.headers_mut() = parts.headers.to_http_new();
        // We cannot do anything about extensions because we cannot iterate over them.

        request
    }
}

impl ToHttpNew for http_old::Method {
    type Output = http::Method;

    fn to_http_new(self) -> Self::Output {
        self.as_str().parse().expect("method should be valid between both versions")
    }
}

impl ToHttpNew for http_old::Uri {
    type Output = http::Uri;

    fn to_http_new(self) -> Self::Output {
        self.to_string().parse().expect("URI should be valid between both versions")
    }
}

impl ToHttpNew for http_old::Version {
    type Output = http::Version;

    fn to_http_new(self) -> Self::Output {
        if self == http_old::Version::HTTP_09 {
            http::Version::HTTP_09
        } else if self == http_old::Version::HTTP_10 {
            http::Version::HTTP_10
        } else if self == http_old::Version::HTTP_11 {
            http::Version::HTTP_11
        } else if self == http_old::Version::HTTP_2 {
            http::Version::HTTP_2
        } else if self == http_old::Version::HTTP_3 {
            http::Version::HTTP_3
        } else {
            // Current http code doesn't have other variants.
            unreachable!()
        }
    }
}

impl ToHttpNew for http_old::HeaderMap<http_old::HeaderValue> {
    type Output = http::HeaderMap<http::HeaderValue>;

    fn to_http_new(self) -> Self::Output {
        let mut map = http::HeaderMap::new();
        map.extend(
            self.into_iter()
                .map(|(name, value)| (name.map(ToHttpNew::to_http_new), value.to_http_new())),
        );

        map
    }
}

impl ToHttpNew for http_old::HeaderName {
    type Output = http::HeaderName;

    fn to_http_new(self) -> Self::Output {
        http::HeaderName::from_bytes(self.as_ref())
            .expect("header name should be valid between both versions")
    }
}

impl ToHttpNew for http_old::HeaderValue {
    type Output = http::HeaderValue;

    fn to_http_new(self) -> Self::Output {
        http::HeaderValue::from_bytes(self.as_bytes())
            .expect("header value should be valid between both versions")
    }
}
