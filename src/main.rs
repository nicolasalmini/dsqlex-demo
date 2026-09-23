mod playground;
mod worker;

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::Html,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use worker::{Cmd, RESERVED, WorkerConfig, spawn_worker};

const DEFAULT_EXPR: &str =
    "CASE\n    WHEN country = 'Brazil' THEN amount * 2\n    ELSE amount\nEND";
const REQUIRED: [&str; 2] = ["country", "amount"];
const SAMPLE_ROWS: usize = 250_000;

#[derive(Clone, Serialize)]
pub struct SharedState {
    pub running: bool,
    pub expression: String,
    pub requested_version: u64,
    pub applied_version: u64,
    pub rows: u64,
    pub errors: u64,
    pub cycles: u64,
    pub tps: f64,
    pub source_headers: Vec<String>,
    pub result_headers: Vec<String>,
    pub source_preview: Vec<Vec<String>>,
    pub destination_preview: Vec<Vec<String>>,
    pub input_path: String,
    pub output_paths: Vec<String>,
    pub last_error: String,
}

impl SharedState {
    pub fn new(expression: &str) -> Self {
        SharedState {
            running: false,
            expression: expression.to_string(),
            requested_version: 1,
            applied_version: 0,
            rows: 0,
            errors: 0,
            cycles: 0,
            tps: 0.0,
            source_headers: Vec::new(),
            result_headers: Vec::new(),
            source_preview: Vec::new(),
            destination_preview: Vec::new(),
            input_path: String::new(),
            output_paths: Vec::new(),
            last_error: String::new(),
        }
    }
}

struct App {
    state: Arc<Mutex<SharedState>>,
    tx: Sender<Cmd>,
    ctl: tokio::sync::Mutex<()>,
    output_dir: PathBuf,
    sample_path: PathBuf,
}

fn err(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<serde_json::Value>) {
    (status, Json(serde_json::json!({ "error": msg.into() })))
}

fn ensure_sample(path: &Path, rows: usize) -> std::io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let mut wtr = csv::Writer::from_path(path)?;
    wtr.write_record(["country", "amount", "category"])?;
    let countries = ["Brazil", "Argentina", "Chile"];
    let categories = ["retail", "wholesale", "services"];
    for i in 0..rows {
        let whole = 10 + (i * 37 % 990) as u32;
        let cents = (i * 53 % 100) as u32;
        wtr.write_record([
            countries[i % 3],
            &format!("{}.{:02}", whole, cents),
            categories[i % 3],
        ])?;
    }
    wtr.flush()
}

fn validate_input(path: &Path, output_dir: &Path) -> Result<Vec<String>, String> {
    let canon_in = path
        .canonicalize()
        .map_err(|e| format!("cannot read input path: {}", e))?;
    let canon_out = output_dir
        .canonicalize()
        .unwrap_or_else(|_| output_dir.to_path_buf());
    if canon_in.starts_with(&canon_out) {
        return Err("input path must not be inside the application output directory".into());
    }
    let mut rdr = csv::ReaderBuilder::new()
        .flexible(true)
        .from_path(&canon_in)
        .map_err(|e| format!("cannot open CSV: {}", e))?;
    let headers = rdr
        .headers()
        .map_err(|e| format!("cannot read headers: {}", e))?
        .clone();
    if headers.is_empty() {
        return Err("CSV file is empty or missing headers".into());
    }
    let mut seen = HashSet::new();
    for h in headers.iter() {
        if h.is_empty() {
            return Err("CSV contains an empty header name".into());
        }
        if !seen.insert(h.to_string()) {
            return Err(format!("duplicate header: {:?}", h));
        }
        if RESERVED.contains(&h) {
            return Err(format!(
                "header {:?} collides with reserved output column",
                h
            ));
        }
    }
    for req in REQUIRED {
        if !headers.iter().any(|h| h == req) {
            return Err(format!("missing required header: {:?}", req));
        }
    }
    let mut rec = csv::StringRecord::new();
    match rdr.read_record(&mut rec) {
        Ok(true) => Ok(headers.iter().map(|s| s.to_string()).collect()),
        Ok(false) => Err("CSV has headers but no data rows".into()),
        Err(e) => Err(format!("cannot read data rows: {}", e)),
    }
}

async fn get_state(State(app): State<Arc<App>>) -> Json<SharedState> {
    Json(app.state.lock().unwrap().clone())
}

#[derive(Deserialize)]
struct RuleBody {
    expression: String,
}

async fn post_rule(
    State(app): State<Arc<App>>,
    Json(body): Json<RuleBody>,
) -> Result<Json<SharedState>, (StatusCode, Json<serde_json::Value>)> {
    let expr = body.expression.trim().to_string();
    if expr.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "expression is empty"));
    }
    if let Err(e) = dsqlex::parse(&expr) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!("invalid expression: {}", e),
        ));
    }
    {
        let mut s = app.state.lock().unwrap();
        s.requested_version += 1;
        s.expression = expr.clone();
        let _ = app.tx.send(Cmd::SetRule {
            expr,
            version: s.requested_version,
        });
    }
    Ok(Json(app.state.lock().unwrap().clone()))
}

#[derive(Deserialize)]
struct StartBody {
    path: Option<String>,
}

fn release_start(app: &App, last_error: Option<String>) {
    let mut s = app.state.lock().unwrap();
    s.running = false;
    if let Some(e) = last_error {
        s.last_error = e;
    }
}

async fn post_start(
    State(app): State<Arc<App>>,
    Json(body): Json<StartBody>,
) -> Result<Json<SharedState>, (StatusCode, Json<serde_json::Value>)> {
    let _ctl = app.ctl.lock().await;
    let path = match body.path.as_deref().map(str::trim) {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => app.sample_path.clone(),
    };
    {
        let mut s = app.state.lock().unwrap();
        if s.running {
            return Err(err(StatusCode::CONFLICT, "pipeline is already running"));
        }
        s.running = true;
    }
    if path == app.sample_path
        && let Err(e) = ensure_sample(&app.sample_path, SAMPLE_ROWS)
    {
        release_start(&app, None);
        return Err(err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
    }
    if let Err(e) = validate_input(&path, &app.output_dir) {
        release_start(&app, None);
        return Err(err(StatusCode::BAD_REQUEST, e));
    }
    let (ack, rx) = tokio::sync::oneshot::channel();
    if app.tx.send(Cmd::Start { path, ack }).is_err() {
        release_start(&app, Some("worker thread unavailable".into()));
        return Err(err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "worker thread unavailable",
        ));
    }
    match rx.await {
        Ok(Ok(())) => Ok(Json(app.state.lock().unwrap().clone())),
        Ok(Err(e)) => {
            release_start(&app, Some(e.clone()));
            Err(err(StatusCode::INTERNAL_SERVER_ERROR, e))
        }
        Err(_) => {
            release_start(&app, Some("worker thread dropped start".into()));
            Err(err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "worker thread dropped start",
            ))
        }
    }
}

async fn post_stop(State(app): State<Arc<App>>) -> Json<SharedState> {
    let _ctl = app.ctl.lock().await;
    let (ack, rx) = tokio::sync::oneshot::channel();
    let _ = app.tx.send(Cmd::Stop { ack: Some(ack) });
    let _ = rx.await;
    Json(app.state.lock().unwrap().clone())
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../static/index.html"))
}

async fn playground_page() -> Html<&'static str> {
    Html(include_str!("../static/playground.html"))
}

async fn templates_js() -> ([(&'static str, &'static str); 1], &'static str) {
    (
        [("content-type", "text/javascript; charset=utf-8")],
        include_str!("../static/templates.js"),
    )
}

#[derive(Deserialize)]
struct PlaygroundBody {
    expression: String,
    context_json: String,
}

async fn post_playground_evaluate(Json(body): Json<PlaygroundBody>) -> Json<serde_json::Value> {
    Json(playground::evaluate(&body.expression, &body.context_json))
}

fn build_app(
    state: Arc<Mutex<SharedState>>,
    output_dir: PathBuf,
    sample_path: PathBuf,
) -> Arc<App> {
    let tx = spawn_worker(
        state.clone(),
        WorkerConfig {
            output_dir: output_dir.clone(),
            rollover_records: 50_000,
            flush_every: 1024,
            batch: 64,
        },
        DEFAULT_EXPR,
    );
    Arc::new(App {
        state,
        tx,
        ctl: tokio::sync::Mutex::new(()),
        output_dir,
        sample_path,
    })
}

fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/playground", get(playground_page))
        .route("/templates.js", get(templates_js))
        .route("/api/playground/evaluate", post(post_playground_evaluate))
        .route("/api/state", get(get_state))
        .route("/api/rule", post(post_rule))
        .route("/api/start", post(post_start))
        .route("/api/stop", post(post_stop))
        .with_state(app)
}

#[tokio::main]
async fn main() {
    let cfg_dir =
        worker::unique_output_dir(Path::new("data")).expect("cannot create output directory");
    let sample_path = PathBuf::from("data").join("sample-250k.csv");
    if let Err(e) = ensure_sample(&sample_path, SAMPLE_ROWS) {
        eprintln!("warning: could not generate sample CSV: {}", e);
    }
    let state = Arc::new(Mutex::new(SharedState::new(DEFAULT_EXPR)));
    let app = build_app(state, cfg_dir, sample_path);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000")
        .await
        .expect("bind 127.0.0.1:3000");
    println!("listening on http://127.0.0.1:3000");
    axum::serve(listener, router(app)).await.unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use std::io::Write;
    use tower::ServiceExt;

    fn outdir(base: &Path) -> PathBuf {
        let d = base.join("out");
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn rejects_bad_csvs() {
        let tmp = tempfile::tempdir().unwrap();
        let out = outdir(tmp.path());

        let empty = tmp.path().join("empty.csv");
        std::fs::write(&empty, "").unwrap();
        assert!(validate_input(&empty, &out).is_err());

        let no_rows = tmp.path().join("norows.csv");
        std::fs::write(&no_rows, "country,amount\n").unwrap();
        assert!(validate_input(&no_rows, &out).is_err());

        let missing = tmp.path().join("missing.csv");
        std::fs::write(&missing, "country,other\nBrazil,1\n").unwrap();
        assert!(validate_input(&missing, &out).is_err());

        let dup = tmp.path().join("dup.csv");
        std::fs::write(&dup, "country,amount,amount\nBrazil,1,2\n").unwrap();
        assert!(validate_input(&dup, &out).is_err());

        let reserved = tmp.path().join("reserved.csv");
        std::fs::write(&reserved, "country,amount,result\nBrazil,1,x\n").unwrap();
        assert!(validate_input(&reserved, &out).is_err());

        let inside = out.join("slot-0.csv");
        std::fs::write(&inside, "country,amount\nBrazil,1\n").unwrap();
        assert!(validate_input(&inside, &out).is_err());
    }

    #[test]
    fn accepts_good_csv_and_sample_is_deterministic() {
        let tmp = tempfile::tempdir().unwrap();
        let out = outdir(tmp.path());
        let good = tmp.path().join("good.csv");
        let mut f = std::fs::File::create(&good).unwrap();
        writeln!(f, "country,amount,extra").unwrap();
        writeln!(f, "Brazil,12.5,x").unwrap();
        drop(f);
        let headers = validate_input(&good, &out).unwrap();
        assert_eq!(headers, vec!["country", "amount", "extra"]);

        let s1 = tmp.path().join("a").join("sample.csv");
        let s2 = tmp.path().join("b").join("sample.csv");
        ensure_sample(&s1, 100).unwrap();
        ensure_sample(&s2, 100).unwrap();
        assert_eq!(std::fs::read(&s1).unwrap(), std::fs::read(&s2).unwrap());
        let content = std::fs::read_to_string(&s1).unwrap();
        assert_eq!(content.lines().count(), 100 + 1);
        assert!(content.contains("Brazil"));
        assert!(content.contains("Argentina"));
        assert!(content.contains("Chile"));
        let before = std::fs::read(&s1).unwrap();
        ensure_sample(&s1, 100).unwrap();
        assert_eq!(std::fs::read(&s1).unwrap(), before);
    }

    async fn call(
        app: Arc<App>,
        method: &str,
        uri: &str,
        body: &str,
    ) -> (StatusCode, serde_json::Value) {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let res = router(app).oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or_default())
    }

    #[tokio::test]
    async fn double_start_conflicts_and_restart_works() {
        let tmp = tempfile::tempdir().unwrap();
        let out = outdir(tmp.path());
        let input = tmp.path().join("in.csv");
        std::fs::write(&input, "country,amount\nBrazil,5\n").unwrap();
        let state = Arc::new(Mutex::new(SharedState::new(DEFAULT_EXPR)));
        let app = build_app(state.clone(), out, tmp.path().join("sample.csv"));

        let body = serde_json::json!({ "path": input.display().to_string() }).to_string();
        let (st, _) = call(app.clone(), "POST", "/api/start", &body).await;
        assert_eq!(st, StatusCode::OK);
        let (st, v) = call(app.clone(), "POST", "/api/start", &body).await;
        assert_eq!(st, StatusCode::CONFLICT);
        assert!(v["error"].as_str().unwrap().contains("already running"));
        let (st, _) = call(app.clone(), "POST", "/api/stop", "{}").await;
        assert_eq!(st, StatusCode::OK);
        assert!(!state.lock().unwrap().running);
        let (st, _) = call(app.clone(), "POST", "/api/start", &body).await;
        assert_eq!(st, StatusCode::OK);
        let (st, _) = call(app.clone(), "POST", "/api/stop", "{}").await;
        assert_eq!(st, StatusCode::OK);
    }

    #[tokio::test]
    async fn concurrent_start_and_stop_are_serialized() {
        let tmp = tempfile::tempdir().unwrap();
        let out = outdir(tmp.path());
        let input = tmp.path().join("in.csv");
        std::fs::write(&input, "country,amount\nBrazil,5\n").unwrap();
        let state = Arc::new(Mutex::new(SharedState::new(DEFAULT_EXPR)));
        let app = build_app(state.clone(), out, tmp.path().join("sample.csv"));
        let body = serde_json::json!({ "path": input.display().to_string() }).to_string();

        let a = app.clone();
        let b = app.clone();
        let c = app.clone();
        let body2 = body.clone();
        let (r1, r2, r3) = tokio::join!(
            call(a, "POST", "/api/start", &body),
            call(b, "POST", "/api/stop", "{}"),
            call(c, "POST", "/api/start", &body2),
        );
        for s in [r1.0, r3.0] {
            assert!(
                s == StatusCode::OK || s == StatusCode::CONFLICT,
                "unexpected start status {}",
                s
            );
        }
        assert!(
            r1.0 == StatusCode::OK || r3.0 == StatusCode::OK,
            "no start succeeded: r1={:?} r3={:?}",
            r1.0,
            r3.0
        );
        assert_eq!(r2.0, StatusCode::OK);
        let (st, _) = call(app.clone(), "POST", "/api/stop", "{}").await;
        assert_eq!(st, StatusCode::OK);
        assert!(!state.lock().unwrap().running);
    }

    #[tokio::test]
    async fn concurrent_rule_updates_keep_versions_ordered() {
        let tmp = tempfile::tempdir().unwrap();
        let out = outdir(tmp.path());
        let state = Arc::new(Mutex::new(SharedState::new(DEFAULT_EXPR)));
        let app = build_app(state.clone(), out, tmp.path().join("sample.csv"));
        let e2 = serde_json::json!({ "expression": "CASE WHEN country = 'Argentina' THEN amount * 3 ELSE amount END" }).to_string();
        let e3 = serde_json::json!({ "expression": "CASE WHEN country = 'Chile' THEN amount * 4 ELSE amount END" }).to_string();
        let a = app.clone();
        let b = app.clone();
        let (r1, r2) = tokio::join!(
            call(a, "POST", "/api/rule", &e2),
            call(b, "POST", "/api/rule", &e3),
        );
        assert_eq!(r1.0, StatusCode::OK);
        assert_eq!(r2.0, StatusCode::OK);
        assert_eq!(state.lock().unwrap().requested_version, 3);
    }

    #[tokio::test]
    async fn empty_start_path_uses_default_sample() {
        let tmp = tempfile::tempdir().unwrap();
        let out = outdir(tmp.path());
        let sample = tmp.path().join("sample-250k.csv");
        ensure_sample(&sample, 100).unwrap();
        let state = Arc::new(Mutex::new(SharedState::new(DEFAULT_EXPR)));
        let app = build_app(state.clone(), out, sample.clone());
        let (st, v) = call(app.clone(), "POST", "/api/start", "{}").await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(
            v["input_path"].as_str().unwrap(),
            sample.display().to_string()
        );
        assert_eq!(
            v["source_headers"],
            serde_json::json!(["country", "amount", "category"])
        );
        let _ = call(app, "POST", "/api/stop", "{}").await;
    }

    #[test]
    fn default_expr_is_multiline_and_parses() {
        assert!(DEFAULT_EXPR.contains('\n'));
        assert!(DEFAULT_EXPR.contains("    WHEN"));
        dsqlex::parse(DEFAULT_EXPR).unwrap();
    }

    const PG_CTX: &str = r#"{
  "country": "Brazil",
  "amount": 125.50,
  "quantity": 4,
  "customer_tier": "gold",
  "status": "active",
  "discount": null,
  "config": { "region": "LATAM", "pricing": { "margin": 0.15, "minimum": 25 } }
}"#;

    async fn pg_eval(app: Arc<App>, expr: &str, ctx: &str) -> serde_json::Value {
        let body = serde_json::json!({ "expression": expr, "context_json": ctx }).to_string();
        let (st, v) = call(app, "POST", "/api/playground/evaluate", &body).await;
        assert_eq!(st, StatusCode::OK);
        v
    }

    #[tokio::test]
    async fn playground_evaluate_cases() {
        let tmp = tempfile::tempdir().unwrap();
        let out = outdir(tmp.path());
        let state = Arc::new(Mutex::new(SharedState::new(DEFAULT_EXPR)));
        let app = build_app(state.clone(), out, tmp.path().join("sample.csv"));

        let r = pg_eval(
            app.clone(),
            "CASE WHEN country = 'Brazil' THEN amount * 2 ELSE amount END",
            PG_CTX,
        )
        .await;
        assert_eq!(r["status"], "ok");
        assert_eq!(r["value"], "251.00");
        assert_eq!(r["value_type"], "decimal");

        let r = pg_eval(
            app.clone(),
            "ROUND((amount * quantity) * (1 + config.pricing.margin), 2)",
            PG_CTX,
        )
        .await;
        assert_eq!(r["value"], "577.30");

        let r = pg_eval(app.clone(), "COALESCE(discount, 0)", PG_CTX).await;
        assert_eq!(r["value"], "0");

        assert_eq!(
            pg_eval(app.clone(), "CASE WHEN ((", PG_CTX).await["status"],
            "syntax_error"
        );
        assert_eq!(
            pg_eval(app.clone(), "nope + 1", PG_CTX).await["status"],
            "evaluation_error"
        );
        assert_eq!(
            pg_eval(app.clone(), "amount", "{bad").await["status"],
            "context_error"
        );
        assert_eq!(
            pg_eval(app.clone(), "amount", "[1,2]").await["status"],
            "context_error"
        );
        assert_eq!(
            pg_eval(app.clone(), "x", r#"{"x": 1e30}"#).await["status"],
            "context_error"
        );

        let r = pg_eval(
            app.clone(),
            "rate",
            r#"{ "rate": 0.1234567890123456789012345678 }"#,
        )
        .await;
        assert_eq!(r["value"], "0.1234567890123456789012345678");
        let r = pg_eval(app.clone(), "x", r#"{ "x": 1e-3 }"#).await;
        assert_eq!(r["value"], "0.001");

        let s = state.lock().unwrap();
        assert!(!s.running);
        assert_eq!(s.requested_version, 1);
        assert_eq!(s.rows, 0);
        drop(s);
    }

    #[tokio::test]
    async fn playground_page_and_nav() {
        let tmp = tempfile::tempdir().unwrap();
        let out = outdir(tmp.path());
        let state = Arc::new(Mutex::new(SharedState::new(DEFAULT_EXPR)));
        let app = build_app(state, out, tmp.path().join("sample.csv"));
        let req = Request::builder()
            .uri("/playground")
            .body(Body::empty())
            .unwrap();
        let res = router(app.clone()).oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        let html = String::from_utf8_lossy(&body);
        assert!(html.contains("Playground"));
        assert!(html.contains("/api/playground/evaluate"));
        assert!(html.contains("src=\"/templates.js\""));

        let req = Request::builder()
            .uri("/templates.js")
            .body(Body::empty())
            .unwrap();
        let res = router(app.clone()).oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let ct = res
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(ct.contains("javascript"));
        let body = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&body).contains("DSQLEX_TEMPLATES"));

        let req = Request::builder().uri("/").body(Body::empty()).unwrap();
        let res = router(app).oneshot(req).await.unwrap();
        let body = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        let html = String::from_utf8_lossy(&body);
        assert!(html.contains("href=\"/playground\""));
        assert!(html.contains("src=\"/templates.js\""));
    }

    #[tokio::test]
    async fn start_releases_reservation_on_bad_path() {
        let tmp = tempfile::tempdir().unwrap();
        let out = outdir(tmp.path());
        let state = Arc::new(Mutex::new(SharedState::new(DEFAULT_EXPR)));
        let app = build_app(state.clone(), out, tmp.path().join("sample.csv"));
        let body = serde_json::json!({ "path": tmp.path().join("nope.csv").display().to_string() })
            .to_string();
        let (st, _) = call(app.clone(), "POST", "/api/start", &body).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert!(!state.lock().unwrap().running);
        let good = tmp.path().join("g.csv");
        std::fs::write(&good, "country,amount\nBrazil,5\n").unwrap();
        let body = serde_json::json!({ "path": good.display().to_string() }).to_string();
        let (st, _) = call(app.clone(), "POST", "/api/start", &body).await;
        assert_eq!(st, StatusCode::OK);
        let _ = call(app, "POST", "/api/stop", "{}").await;
    }
}
