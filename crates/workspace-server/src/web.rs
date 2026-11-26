use axum::{
    Router,
    extract::Path,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "../../web/dist/"]
struct Assets;

fn serve_asset(path: &str) -> Response {
    let Some(file) = Assets::get(path) else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let mime = mime_guess::from_path(path).first_or_octet_stream();

    (
        [
            (header::CONTENT_TYPE, mime.essence_str()),
            (header::CACHE_CONTROL, "no-cache"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        file.data.into_owned(),
    )
        .into_response()
}

async fn index() -> Response {
    serve_asset("index.html")
}

async fn asset(Path(path): Path<String>) -> Response {
    serve_asset(&format!("assets/{path}"))
}

/// Bundled brand artwork and fonts live at the root of `dist` (Vite copies
/// `public/` there). Only these closed prefixes are exposed; everything else
/// still returns 404.
async fn brand(Path(path): Path<String>) -> Response {
    serve_asset(&format!("brand/{path}"))
}

async fn fonts(Path(path): Path<String>) -> Response {
    serve_asset(&format!("fonts/{path}"))
}

async fn licenses(Path(path): Path<String>) -> Response {
    serve_asset(&format!("licenses/{path}"))
}

async fn favicon() -> Response {
    serve_asset("favicon.svg")
}

async fn theme_boot() -> Response {
    serve_asset("theme-boot.js")
}

pub fn router() -> Router {
    Router::new()
        .route("/", get(index))
        .route("/assets/{*path}", get(asset))
        .route("/brand/{*path}", get(brand))
        .route("/fonts/{*path}", get(fonts))
        .route("/licenses/{*path}", get(licenses))
        .route("/favicon.svg", get(favicon))
        .route("/theme-boot.js", get(theme_boot))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    async fn request(path: &str) -> Response {
        router()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn serves_embedded_index() {
        let response = request("/").await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "text/html");
    }

    #[tokio::test]
    async fn serves_embedded_javascript() {
        let path = Assets::iter()
            .find(|path| path.starts_with("assets/") && path.ends_with(".js"))
            .expect("Vite JavaScript bundle is missing");

        let response = request(&format!("/{path}")).await;

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn missing_asset_returns_not_found() {
        let response = request("/assets/missing.js").await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn serves_bundled_brand_fonts_and_boot_script() {
        let expected = [
            ("brand/logo-mark.svg", "image/svg+xml"),
            ("brand/logo-adaptive.svg", "image/svg+xml"),
            ("fonts/Geist-Variable.woff2", "font/woff2"),
            ("favicon.svg", "image/svg+xml"),
            ("theme-boot.js", "text/javascript"),
        ];

        for (path, content_type) in expected {
            assert!(
                Assets::get(path).is_some(),
                "{path} is missing from the embedded build"
            );

            let response = request(&format!("/{path}")).await;

            assert_eq!(response.status(), StatusCode::OK, "{path}");
            assert_eq!(
                response.headers()[header::CONTENT_TYPE],
                content_type,
                "{path}"
            );
        }
    }

    #[tokio::test]
    async fn root_level_files_are_not_a_catch_all() {
        // Only the closed allowlist is served; arbitrary root paths stay 404.
        assert_eq!(
            request("/config.toml").await.status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            request("/brand/../index.html").await.status(),
            StatusCode::NOT_FOUND
        );
    }
}
