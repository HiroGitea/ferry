//! Serves the dashboard (web/dist) embedded at build time with `--features embed-web`.

use std::sync::Arc;

use axum::Router;

use crate::state::AppState;

#[cfg(feature = "embed-web")]
mod embedded {
    use axum::http::{StatusCode, Uri, header};
    use axum::response::{IntoResponse, Response};

    #[derive(rust_embed::Embed)]
    #[folder = "../../web/dist/"]
    struct Assets;

    pub async fn serve(uri: Uri) -> Response {
        let path = uri.path().trim_start_matches('/');
        let path = if path.is_empty() { "index.html" } else { path };
        match Assets::get(path) {
            Some(file) => {
                let mime = mime_guess::from_path(path).first_or_octet_stream();
                (
                    [(header::CONTENT_TYPE, mime.as_ref().to_string())],
                    file.data.into_owned(),
                )
                    .into_response()
            }
            None => match Assets::get("index.html") {
                // SPA fallback
                Some(index) => (
                    [(header::CONTENT_TYPE, "text/html".to_string())],
                    index.data.into_owned(),
                )
                    .into_response(),
                None => (StatusCode::NOT_FOUND, "dashboard not built").into_response(),
            },
        }
    }
}

pub fn mount(app: Router<Arc<AppState>>) -> Router<Arc<AppState>> {
    #[cfg(feature = "embed-web")]
    {
        app.fallback(embedded::serve)
    }
    #[cfg(not(feature = "embed-web"))]
    {
        app.fallback(|| async {
            (
                axum::http::StatusCode::NOT_FOUND,
                "mlqd was built without the embedded dashboard (cargo build --features embed-web). The API is at /api/v1.",
            )
        })
    }
}
