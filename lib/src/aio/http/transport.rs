//! Bounded idempotent request retries and portable endpoint construction.
use super::error::Error;
use rdedup_protocol::credential::BearerToken;
use rdedup_protocol::problem::Problem;
use rdedup_protocol::route;
use reqwest::blocking::{Client, RequestBuilder, Response};
use reqwest::header::AUTHORIZATION;
use std::time::Duration;
use url::Url;

#[derive(Clone)]
pub(super) struct Transport {
    client: Client,
    base: Url,
    token: Option<BearerToken>,
}
#[derive(Clone, Copy)]
pub(super) enum Retry {
    Idempotent,
    Never,
}
impl Transport {
    pub(super) fn new(
        base: Url,
        token: Option<BearerToken>,
    ) -> Result<Self, Error> {
        if !matches!(base.scheme(), "http" | "https")
            || base.host_str().is_none()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return Err(Error::RepositoryUrl);
        }
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            client,
            base,
            token,
        })
    }
    pub(super) fn endpoint(
        &self,
        route: &str,
        suffix: Option<&str>,
    ) -> Result<Url, Error> {
        let mut url = self.base.clone();
        {
            let mut segments =
                url.path_segments_mut().map_err(|_| Error::RepositoryUrl)?;
            segments.pop_if_empty();
            segments.extend(route.trim_start_matches('/').split('/'));
            if let Some(suffix) = suffix {
                segments.extend(suffix.split('/'));
            }
        }
        Ok(url)
    }
    pub(super) fn request(
        &self,
        method: reqwest::Method,
        url: Url,
    ) -> RequestBuilder {
        let request = self.client.request(method, url);
        match &self.token {
            Some(token) => {
                request.header(AUTHORIZATION, token.authorization_header())
            }
            None => request,
        }
    }
    pub(super) fn execute(
        &self,
        retry: Retry,
        mut request: impl FnMut() -> Result<RequestBuilder, Error>,
    ) -> Result<Response, Error> {
        let attempts = match retry {
            Retry::Idempotent => 3,
            Retry::Never => 1,
        };
        for attempt in 0..attempts {
            let result = request()?
                .send()
                .map_err(Error::from)
                .and_then(Self::response);
            match result {
                Err(error)
                    if attempt + 1 < attempts && error.is_transient() =>
                {
                    std::thread::sleep(Duration::from_millis(
                        100 * (attempt + 1),
                    ));
                }
                result => return result,
            }
        }
        unreachable!("every nonempty retry loop returns its final result")
    }
    fn response(response: Response) -> Result<Response, Error> {
        if response.status().is_success() {
            return Ok(response);
        }
        // HEAD cannot carry a body; every server error supplies this same record.
        let encoded = response
            .headers()
            .get(route::PROBLEM_HEADER)
            .ok_or(Error::Protocol)?;
        let problem: Problem = serde_json::from_slice(encoded.as_bytes())
            .map_err(|_| Error::Protocol)?;
        Err(Error::Problem(problem.kind))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repository_prefix_and_object_segments_remain_distinct() {
        let transport = Transport::new(
            "https://example.test/repository".parse().unwrap(),
            None,
        )
        .unwrap();
        assert_eq!(transport.endpoint(route::OBJECTS, Some("0/name/a %23.yml")).unwrap().as_str(), "https://example.test/repository/api/v1/objects/0/name/a%20%2523.yml");
        for url in [
            "file:///repository",
            "https://token@example.test",
            "https://example.test/?secret=value",
            "https://example.test/#fragment",
        ] {
            assert!(Transport::new(url.parse().unwrap(), None).is_err());
        }
    }

    enum Reply {
        Success,
        Problem(rdedup_protocol::problem::ProblemKind),
    }

    fn assert_attempts(
        replies: Vec<Reply>,
        retry: Retry,
        expected: Result<(), rdedup_protocol::problem::ProblemKind>,
    ) {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;
        use std::time::Instant;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let expected_attempts = replies.len();
        let responder = std::thread::spawn(move || {
            for reply in replies {
                let deadline = Instant::now() + Duration::from_secs(10);
                let mut connection = loop {
                    match listener.accept() {
                        Ok((connection, _)) => break connection,
                        Err(error)
                            if error.kind()
                                == std::io::ErrorKind::WouldBlock =>
                        {
                            assert!(
                                Instant::now() < deadline,
                                "expected request never arrived"
                            );
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        Err(error) => panic!("accept failed: {error}"),
                    }
                };
                connection.set_nonblocking(false).unwrap();
                connection
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(&connection);
                loop {
                    let mut line = String::new();
                    assert_ne!(reader.read_line(&mut line).unwrap(), 0);
                    if line == "\r\n" {
                        break;
                    }
                }
                let response = match reply {
                    Reply::Success => "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned(),
                    Reply::Problem(kind) => {
                        let problem = serde_json::to_string(&Problem {
                            kind,
                            detail: "injected response".to_owned(),
                        }).unwrap();
                        format!("HTTP/1.1 {}\r\n{}: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", kind.status(), route::PROBLEM_HEADER, problem)
                    }
                };
                connection.write_all(response.as_bytes()).unwrap();
            }
        });
        let transport = Transport::new(address.parse().unwrap(), None).unwrap();
        let mut attempts = 0;
        let outcome = transport.execute(retry, || {
            attempts += 1;
            Ok(transport
                .request(reqwest::Method::GET, address.parse().unwrap()))
        });
        responder.join().unwrap();
        assert_eq!(attempts, expected_attempts);
        match (outcome, expected) {
            (Ok(_), Ok(())) => {}
            (Err(Error::Problem(actual)), Err(expected)) => {
                assert_eq!(actual, expected)
            }
            (actual, expected) => {
                panic!("unexpected result: {actual:?}; expected {expected:?}")
            }
        }
    }

    #[test]
    fn idempotent_requests_recover_transient_failures_with_at_most_three_attempts(
    ) {
        use rdedup_protocol::problem::ProblemKind::Unavailable;
        assert_attempts(
            vec![Reply::Problem(Unavailable), Reply::Success],
            Retry::Idempotent,
            Ok(()),
        );
        assert_attempts(
            vec![
                Reply::Problem(Unavailable),
                Reply::Problem(Unavailable),
                Reply::Problem(Unavailable),
            ],
            Retry::Idempotent,
            Err(Unavailable),
        );
    }

    #[test]
    fn permanent_problems_and_non_idempotent_requests_are_not_retried() {
        use rdedup_protocol::problem::ProblemKind;
        for kind in [
            ProblemKind::InvalidRequest,
            ProblemKind::Unauthorized,
            ProblemKind::Forbidden,
            ProblemKind::AlreadyExists,
            ProblemKind::RequestConflict,
            ProblemKind::LeaseExpired,
            ProblemKind::RenewalClosed,
            ProblemKind::LeaseMode,
            ProblemKind::StorageFailure,
        ] {
            assert_attempts(
                vec![Reply::Problem(kind)],
                Retry::Idempotent,
                Err(kind),
            );
        }
        assert_attempts(
            vec![Reply::Problem(ProblemKind::Unavailable)],
            Retry::Never,
            Err(ProblemKind::Unavailable),
        );
    }
}
