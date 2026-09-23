use crate::SharedState;
use dsqlex::evaluator::{Context, Value};
use rust_decimal::Decimal;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

const PREVIEW_CAP: usize = 12;
pub const RESERVED: [&str; 4] = ["result", "rule_version", "error", "cycle"];

#[derive(Debug)]
pub enum Cmd {
    Start {
        path: PathBuf,
        ack: oneshot::Sender<Result<(), String>>,
    },
    Stop {
        ack: Option<oneshot::Sender<()>>,
    },
    SetRule {
        expr: String,
        version: u64,
    },
}

#[derive(Clone)]
pub struct WorkerConfig {
    pub output_dir: PathBuf,
    pub rollover_records: u64,
    pub flush_every: u64,
    pub batch: usize,
}

pub fn unique_output_dir(base: &std::path::Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(base)
        .map_err(|e| format!("cannot create {}: {}", base.display(), e))?;
    let pid = std::process::id();
    for i in 0..10_000u32 {
        let dir = base.join(format!("output-{}-{}", pid, i));
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("cannot create {}: {}", dir.display(), e)),
        }
    }
    Err(format!(
        "could not allocate a unique output directory under {}",
        base.display()
    ))
}

impl Default for WorkerConfig {
    fn default() -> Self {
        WorkerConfig {
            output_dir: unique_output_dir(std::path::Path::new("data"))
                .expect("cannot create output directory"),
            rollover_records: 50_000,
            flush_every: 1024,
            batch: 64,
        }
    }
}

pub fn slot_path(output_dir: &std::path::Path, slot: usize) -> PathBuf {
    output_dir.join(format!("slot-{}.csv", slot))
}

pub fn spawn_worker(
    state: Arc<Mutex<SharedState>>,
    cfg: WorkerConfig,
    initial_expr: &str,
) -> Sender<Cmd> {
    let (tx, rx) = std::sync::mpsc::channel();
    let expr = initial_expr.to_string();
    std::thread::spawn(move || run(rx, state, cfg, expr));
    tx
}

struct Pipe {
    input_path: PathBuf,
    rdr: csv::Reader<std::fs::File>,
    headers: Vec<String>,
    wtr: csv::Writer<std::fs::File>,
    slot: usize,
    slot_rows: u64,
    since_flush: u64,
}

impl Pipe {
    fn open(path: PathBuf, cfg: &WorkerConfig) -> Result<Pipe, String> {
        let mut rdr = csv::ReaderBuilder::new()
            .flexible(true)
            .from_path(&path)
            .map_err(|e| format!("cannot open input: {}", e))?;
        let headers: Vec<String> = rdr
            .headers()
            .map_err(|e| format!("cannot read headers: {}", e))?
            .iter()
            .map(|s| s.to_string())
            .collect();
        if headers.is_empty() {
            return Err("input has no headers".into());
        }
        let file = std::fs::File::create(slot_path(&cfg.output_dir, 0))
            .map_err(|e| format!("cannot create output slot: {}", e))?;
        let mut p = Pipe {
            input_path: path,
            rdr,
            headers,
            wtr: csv::Writer::from_writer(file),
            slot: 0,
            slot_rows: 0,
            since_flush: 0,
        };
        p.write_header()?;
        Ok(p)
    }

    fn write_header(&mut self) -> Result<(), String> {
        let mut header = self.headers.clone();
        header.extend(RESERVED.iter().map(|s| s.to_string()));
        self.wtr.write_record(&header).map_err(|e| e.to_string())?;
        self.wtr.flush().map_err(|e| e.to_string())
    }

    fn rotate(&mut self, cfg: &WorkerConfig) -> Result<(), String> {
        self.wtr.flush().map_err(|e| e.to_string())?;
        self.slot = (self.slot + 1) % 2;
        let file = std::fs::File::create(slot_path(&cfg.output_dir, self.slot))
            .map_err(|e| e.to_string())?;
        self.wtr = csv::Writer::from_writer(file);
        self.slot_rows = 0;
        self.write_header()
    }

    fn reopen_input(&mut self) -> Result<(), String> {
        let mut rdr = csv::ReaderBuilder::new()
            .flexible(true)
            .from_path(&self.input_path)
            .map_err(|e| format!("cannot reopen input: {}", e))?;
        let headers: Vec<String> = rdr
            .headers()
            .map_err(|e| format!("cannot re-read headers: {}", e))?
            .iter()
            .map(|s| s.to_string())
            .collect();
        if headers != self.headers {
            return Err("input headers changed or file became empty between cycles".into());
        }
        self.rdr = rdr;
        Ok(())
    }
}

struct Counters {
    rows: u64,
    errors: u64,
    cycles: u64,
    rec_index: u64,
    source_preview: VecDeque<Vec<String>>,
    dest_preview: VecDeque<Vec<String>>,
}

impl Counters {
    fn new() -> Self {
        Counters {
            rows: 0,
            errors: 0,
            cycles: 0,
            rec_index: 0,
            source_preview: VecDeque::new(),
            dest_preview: VecDeque::new(),
        }
    }

    fn push_preview(&mut self, src: Vec<String>, dst: Vec<String>) {
        self.source_preview.push_back(src);
        self.dest_preview.push_back(dst);
        while self.source_preview.len() > PREVIEW_CAP {
            self.source_preview.pop_front();
        }
        while self.dest_preview.len() > PREVIEW_CAP {
            self.dest_preview.pop_front();
        }
    }

    fn sync(&self, state: &Arc<Mutex<SharedState>>) {
        let mut s = state.lock().unwrap();
        s.rows = self.rows;
        s.errors = self.errors;
        s.cycles = self.cycles;
        s.source_preview = self.source_preview.iter().cloned().collect();
        s.destination_preview = self.dest_preview.iter().cloned().collect();
    }
}

fn apply_rule(
    expr: &str,
    version: u64,
    ast: &mut dsqlex::ast::AstNode,
    active_version: &mut u64,
    state: &Arc<Mutex<SharedState>>,
) {
    if version <= *active_version {
        return;
    }
    match dsqlex::parse(expr) {
        Ok(new_ast) => {
            *ast = new_ast;
            *active_version = version;
            state.lock().unwrap().applied_version = version;
        }
        Err(e) => {
            state.lock().unwrap().last_error =
                format!("rule v{} rejected by worker: {}", version, e);
        }
    }
}

fn eval_fields(
    headers: &[String],
    fields: &[String],
    ast: &dsqlex::ast::AstNode,
) -> (String, String) {
    let mut ctx = Context::new();
    for (h, f) in headers.iter().zip(fields.iter()) {
        if h == "amount" {
            match Decimal::from_str(f) {
                Ok(d) => ctx.set("amount", Value::Decimal(d)),
                Err(_) => return (String::new(), format!("invalid amount: {:?}", f)),
            }
        } else {
            ctx.set_string(h.as_str(), f.as_str());
        }
    }
    match dsqlex::eval(ast, &ctx) {
        Ok(v) => (v.to_string(), String::new()),
        Err(e) => (String::new(), e.to_string()),
    }
}

fn run(rx: Receiver<Cmd>, state: Arc<Mutex<SharedState>>, cfg: WorkerConfig, expr: String) {
    let _ = std::fs::create_dir_all(&cfg.output_dir);
    let mut ast = dsqlex::parse(&expr).expect("initial expression must parse");
    let mut active_version = 1u64;
    {
        let mut s = state.lock().unwrap();
        s.applied_version = 1;
        s.output_paths = (0..2)
            .map(|i| slot_path(&cfg.output_dir, i).display().to_string())
            .collect();
    }
    let mut pending: Option<(String, u64)> = None;
    let mut pipe: Option<Pipe> = None;
    let mut running = false;
    let mut counters = Counters::new();
    let mut last_tick = Instant::now();
    let mut rows_at_tick = 0u64;

    let fatal =
        |state: &Arc<Mutex<SharedState>>, counters: &Counters, running: &mut bool, e: String| {
            *running = false;
            counters.sync(state);
            let mut s = state.lock().unwrap();
            s.running = false;
            s.tps = 0.0;
            s.last_error = e;
        };

    loop {
        let cmd = if running {
            match rx.try_recv() {
                Ok(c) => Some(c),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => break,
            }
        } else {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(c) => Some(c),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        };

        match cmd {
            Some(Cmd::SetRule { expr, version }) => {
                let dominated = version <= active_version
                    || pending
                        .as_ref()
                        .map(|(_, v)| version <= *v)
                        .unwrap_or(false);
                if !dominated {
                    pending = Some((expr, version));
                }
            }
            Some(Cmd::Start { path, ack }) => {
                if running {
                    let _ = ack.send(Err("pipeline already running".into()));
                } else {
                    match Pipe::open(path.clone(), &cfg) {
                        Ok(p) => {
                            counters = Counters::new();
                            rows_at_tick = 0;
                            last_tick = Instant::now();
                            {
                                let mut s = state.lock().unwrap();
                                s.running = true;
                                s.input_path = path.display().to_string();
                                s.rows = 0;
                                s.errors = 0;
                                s.cycles = 0;
                                s.tps = 0.0;
                                s.source_preview.clear();
                                s.destination_preview.clear();
                                s.source_headers = p.headers.clone();
                                let mut rh = p.headers.clone();
                                rh.extend(RESERVED.iter().map(|x| x.to_string()));
                                s.result_headers = rh;
                                s.last_error.clear();
                            }
                            pipe = Some(p);
                            running = true;
                            let _ = ack.send(Ok(()));
                        }
                        Err(e) => {
                            state.lock().unwrap().last_error = e.clone();
                            let _ = ack.send(Err(e));
                        }
                    }
                }
            }
            Some(Cmd::Stop { ack }) => {
                if let Some(p) = pipe.as_mut()
                    && let Err(e) = p.wtr.flush()
                {
                    state.lock().unwrap().last_error = format!("flush on stop failed: {}", e);
                }
                running = false;
                counters.sync(&state);
                {
                    let mut s = state.lock().unwrap();
                    s.running = false;
                    s.tps = 0.0;
                }
                if let Some(a) = ack {
                    let _ = a.send(());
                }
            }
            None => {}
        }

        if let Some((e, v)) = pending.take() {
            apply_rule(&e, v, &mut ast, &mut active_version, &state);
        }

        if running && let Some(p) = pipe.as_mut() {
            let mut fatal_err: Option<String> = None;
            let mut rec = csv::StringRecord::new();
            let mut processed = 0usize;
            while processed < cfg.batch {
                match p.rdr.read_record(&mut rec) {
                    Ok(true) => {
                        counters.rec_index += 1;
                        let src: Vec<String> = (0..p.headers.len())
                            .map(|i| rec.get(i).unwrap_or("").to_string())
                            .collect();
                        let (result, err) = if rec.len() != p.headers.len() {
                            (
                                String::new(),
                                format!(
                                    "row {} has {} fields, expected {}",
                                    counters.rec_index,
                                    rec.len(),
                                    p.headers.len()
                                ),
                            )
                        } else {
                            eval_fields(&p.headers, &src, &ast)
                        };
                        if !err.is_empty() {
                            counters.errors += 1;
                        }
                        let mut out = src.clone();
                        out.push(result);
                        out.push(active_version.to_string());
                        out.push(err);
                        out.push(counters.cycles.to_string());
                        match p.wtr.write_record(&out) {
                            Ok(()) => {
                                counters.rows += 1;
                                p.slot_rows += 1;
                                p.since_flush += 1;
                            }
                            Err(e) => {
                                fatal_err = Some(format!("output write failed: {}", e));
                                break;
                            }
                        }
                        counters.push_preview(src, out);
                        processed += 1;
                        if p.slot_rows >= cfg.rollover_records {
                            if let Err(e) = p.rotate(&cfg) {
                                fatal_err = Some(format!("output rotation failed: {}", e));
                                break;
                            }
                            p.since_flush = 0;
                        } else if p.since_flush >= cfg.flush_every {
                            if let Err(e) = p.wtr.flush() {
                                fatal_err = Some(format!("output flush failed: {}", e));
                                break;
                            }
                            p.since_flush = 0;
                        }
                    }
                    Ok(false) => {
                        if counters.rec_index == 0 {
                            fatal_err =
                                Some("input produced zero data rows on a full pass".to_string());
                            break;
                        }
                        counters.cycles += 1;
                        counters.rec_index = 0;
                        if let Err(e) = p.reopen_input() {
                            fatal_err = Some(e);
                            break;
                        }
                    }
                    Err(e) => {
                        counters.errors += 1;
                        state.lock().unwrap().last_error = format!("malformed row: {}", e);
                        processed += 1;
                    }
                }
            }
            counters.sync(&state);
            if let Some(e) = fatal_err {
                if let Some(p) = pipe.as_mut() {
                    let _ = p.wtr.flush();
                }
                fatal(&state, &counters, &mut running, e);
            }
        }

        if last_tick.elapsed() >= Duration::from_secs(1) {
            let elapsed = last_tick.elapsed().as_secs_f64();
            let mut s = state.lock().unwrap();
            if running {
                s.tps = (counters.rows - rows_at_tick) as f64 / elapsed;
            } else {
                s.tps = 0.0;
            }
            rows_at_tick = counters.rows;
            last_tick = Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SharedState;
    use std::io::Write;
    use std::sync::mpsc::Sender;

    fn wait_for<F: Fn(&SharedState) -> bool>(
        state: &Arc<Mutex<SharedState>>,
        f: F,
        timeout: Duration,
    ) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if f(&state.lock().unwrap()) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    fn write_input(dir: &std::path::Path, rows: usize, bad_amount_row: Option<usize>) -> PathBuf {
        let path = dir.join("input.csv");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "country,amount,note").unwrap();
        for i in 0..rows {
            let country = match i % 3 {
                0 => "Brazil",
                1 => "Argentina",
                _ => "Chile",
            };
            let amount = if Some(i) == bad_amount_row {
                "not-a-number".to_string()
            } else {
                format!("{}.50", 10 + i % 90)
            };
            writeln!(f, "{},{},r{}", country, amount, i).unwrap();
        }
        path
    }

    fn setup(
        dir: &std::path::Path,
        rollover: u64,
        batch: usize,
    ) -> (Arc<Mutex<SharedState>>, Sender<Cmd>) {
        let state = Arc::new(Mutex::new(SharedState::new(crate::DEFAULT_EXPR)));
        let cfg = WorkerConfig {
            output_dir: dir.join("out"),
            rollover_records: rollover,
            flush_every: 16,
            batch,
        };
        std::fs::create_dir_all(&cfg.output_dir).unwrap();
        let tx = spawn_worker(state.clone(), cfg, crate::DEFAULT_EXPR);
        (state, tx)
    }

    fn start(tx: &Sender<Cmd>, path: PathBuf) -> oneshot::Receiver<Result<(), String>> {
        let (ack, rx) = oneshot::channel();
        tx.send(Cmd::Start { path, ack }).unwrap();
        rx
    }

    fn stop(tx: &Sender<Cmd>) {
        let (ack, rx) = oneshot::channel();
        tx.send(Cmd::Stop { ack: Some(ack) }).unwrap();
        rx.blocking_recv().unwrap();
    }

    fn read_outputs(dir: &std::path::Path) -> Vec<csv::StringRecord> {
        let mut all = Vec::new();
        for i in 0..2 {
            let p = slot_path(dir, i);
            if p.exists() {
                let mut rdr = csv::ReaderBuilder::new()
                    .flexible(true)
                    .from_path(&p)
                    .unwrap();
                for r in rdr.records().flatten() {
                    all.push(r);
                }
            }
        }
        all
    }

    #[test]
    fn rule_switch_stamps_both_versions_and_math() {
        let tmp = tempfile::tempdir().unwrap();
        let input = write_input(tmp.path(), 300, None);
        let (state, tx) = setup(tmp.path(), 10_000, 8);
        start(&tx, input.clone()).blocking_recv().unwrap().unwrap();
        tx.send(Cmd::SetRule {
            expr: "CASE WHEN country = 'Brazil' THEN amount * 2 WHEN country = 'Argentina' THEN amount * 3 ELSE amount END".into(),
            version: 2,
        })
        .unwrap();
        assert!(wait_for(&state, |s| s.rows >= 250, Duration::from_secs(10)));
        stop(&tx);
        let s = state.lock().unwrap();
        assert_eq!(s.applied_version, 2);
        assert_eq!(s.errors, 0);
        assert!(!s.running);
        drop(s);

        let rows = read_outputs(&tmp.path().join("out"));
        assert!(rows.len() >= 250);
        let v_idx = rows[0].len() - 3;
        let res_idx = rows[0].len() - 4;
        let v1 = rows.iter().filter(|r| &r[v_idx] == "1").count();
        let v2 = rows.iter().filter(|r| &r[v_idx] == "2").count();
        assert!(v1 > 0 && v2 > 0, "v1={} v2={}", v1, v2);
        for r in &rows {
            let amount: f64 = r[1].parse().unwrap();
            let result: f64 = r[res_idx].parse().unwrap();
            let expected = if &r[v_idx] == "1" {
                if r[0] == *"Brazil" {
                    amount * 2.0
                } else {
                    amount
                }
            } else if &r[0] == "Brazil" {
                amount * 2.0
            } else if &r[0] == "Argentina" {
                amount * 3.0
            } else {
                amount
            };
            assert!((result - expected).abs() < 1e-9, "row {:?}", r);
        }
    }

    #[test]
    fn invalid_rule_keeps_previous() {
        let tmp = tempfile::tempdir().unwrap();
        let input = write_input(tmp.path(), 50, None);
        let (state, tx) = setup(tmp.path(), 10_000, 8);
        start(&tx, input).blocking_recv().unwrap().unwrap();
        assert!(wait_for(&state, |s| s.rows > 10, Duration::from_secs(10)));
        tx.send(Cmd::SetRule {
            expr: "CASE WHEN THEN nonsense ((((".into(),
            version: 2,
        })
        .unwrap();
        std::thread::sleep(Duration::from_millis(300));
        stop(&tx);
        let s = state.lock().unwrap();
        assert_eq!(s.applied_version, 1);
        assert!(s.last_error.contains("rejected"));
    }

    fn expected_on_disk(rows: u64, rollover: u64) -> u64 {
        let rem = rows % rollover;
        if rows == 0 {
            0
        } else if rem == 0 {
            rollover.min(rows)
        } else {
            rollover + rem
        }
    }

    fn count_output_rows(dir: &std::path::Path, width: usize) -> u64 {
        let mut written = 0u64;
        for i in 0..2 {
            let p = slot_path(dir, i);
            if !p.exists() {
                continue;
            }
            let mut rdr = csv::ReaderBuilder::new()
                .flexible(true)
                .from_path(&p)
                .unwrap();
            let h: Vec<&str> = rdr.headers().unwrap().iter().collect();
            assert_eq!(
                &h[h.len() - 4..],
                ["result", "rule_version", "error", "cycle"]
            );
            for r in rdr.records().flatten() {
                assert_eq!(r.len(), width, "ragged output row: {:?}", r);
                written += 1;
            }
        }
        written
    }

    #[test]
    fn eof_cycles_and_bounded_output() {
        let tmp = tempfile::tempdir().unwrap();
        let input = write_input(tmp.path(), 40, None);
        let input_bytes = std::fs::read(&input).unwrap();
        let (state, tx) = setup(tmp.path(), 100, 16);
        start(&tx, input.clone()).blocking_recv().unwrap().unwrap();
        assert!(wait_for(&state, |s| s.cycles > 1, Duration::from_secs(10)));
        stop(&tx);
        let total_rows = state.lock().unwrap().rows;
        assert!(total_rows > 100);

        let out = tmp.path().join("out");
        let files: Vec<_> = std::fs::read_dir(&out)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map(|x| x == "csv").unwrap_or(false))
            .collect();
        assert!(files.len() <= 2, "output files: {:?}", files);
        assert_eq!(
            count_output_rows(&out, 7),
            expected_on_disk(total_rows, 100),
            "stop flush lost rows"
        );
        assert_eq!(std::fs::read(&input).unwrap(), input_bytes);
    }

    #[test]
    fn malformed_amount_and_width_errors_continue() {
        let tmp = tempfile::tempdir().unwrap();
        let input = tmp.path().join("input.csv");
        {
            let mut f = std::fs::File::create(&input).unwrap();
            writeln!(f, "country,amount,note").unwrap();
            writeln!(f, "Brazil,10.00,a").unwrap();
            writeln!(f, "Chile,bad,b").unwrap();
            writeln!(f, "Argentina,5.00").unwrap();
            writeln!(f, "Brazil,7.00,x,extra").unwrap();
            writeln!(f, "Chile,3.00,c").unwrap();
        }
        let (state, tx) = setup(tmp.path(), 10_000, 4);
        start(&tx, input).blocking_recv().unwrap().unwrap();
        assert!(wait_for(&state, |s| s.rows >= 20, Duration::from_secs(10)));
        stop(&tx);
        let s = state.lock().unwrap();
        assert!(s.errors >= 3, "errors={}", s.errors);
        let rows = read_outputs(&tmp.path().join("out"));
        for r in &rows {
            assert_eq!(r.len(), 7, "ragged output row: {:?}", r);
        }
        let err_idx = rows[0].len() - 2;
        assert!(rows.iter().any(|r| r[err_idx].contains("fields")));
        assert!(rows.iter().any(|r| r[err_idx].contains("invalid amount")));
    }

    #[test]
    fn duplicate_start_and_start_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let input = write_input(tmp.path(), 20, None);
        let (state, tx) = setup(tmp.path(), 10_000, 8);
        start(&tx, input.clone()).blocking_recv().unwrap().unwrap();
        let second = start(&tx, input).blocking_recv().unwrap();
        assert!(second.is_err());
        stop(&tx);
        let missing = start(&tx, tmp.path().join("nope.csv"))
            .blocking_recv()
            .unwrap();
        assert!(missing.is_err());
        let s = state.lock().unwrap();
        assert!(!s.running);
        assert!(!s.last_error.is_empty());
    }

    #[test]
    fn restart_after_stop_is_clean() {
        let tmp = tempfile::tempdir().unwrap();
        let input = write_input(tmp.path(), 30, None);
        let (state, tx) = setup(tmp.path(), 10_000, 8);
        start(&tx, input.clone()).blocking_recv().unwrap().unwrap();
        assert!(wait_for(
            &state,
            |s| s.rows >= 10_450,
            Duration::from_secs(10)
        ));
        stop(&tx);
        let dir = tmp.path().join("out");
        start(&tx, input).blocking_recv().unwrap().unwrap();
        assert!(wait_for(
            &state,
            |s| s.rows >= 10_350,
            Duration::from_secs(10)
        ));
        stop(&tx);
        let s = state.lock().unwrap();
        assert_eq!(count_output_rows(&dir, 7), expected_on_disk(s.rows, 10_000));
    }

    #[test]
    fn out_of_order_rule_versions_never_regress() {
        let tmp = tempfile::tempdir().unwrap();
        let input = write_input(tmp.path(), 300, None);
        let (state, tx) = setup(tmp.path(), 10_000, 8);
        start(&tx, input.clone()).blocking_recv().unwrap().unwrap();
        tx.send(Cmd::SetRule {
            expr: "CASE WHEN country = 'Argentina' THEN amount * 3 ELSE amount END".into(),
            version: 3,
        })
        .unwrap();
        tx.send(Cmd::SetRule {
            expr: "CASE WHEN country = 'Brazil' THEN amount * 99 ELSE amount END".into(),
            version: 2,
        })
        .unwrap();
        assert!(wait_for(&state, |s| s.rows >= 250, Duration::from_secs(10)));
        stop(&tx);
        let s = state.lock().unwrap();
        assert_eq!(s.applied_version, 3);
        drop(s);

        let rows = read_outputs(&tmp.path().join("out"));
        assert!(!rows.is_empty());
        let v_idx = rows[0].len() - 3;
        let res_idx = rows[0].len() - 4;
        assert!(
            rows.iter().all(|r| &r[v_idx] != "2"),
            "stale v2 stamped rows"
        );
        let mut saw_v3 = false;
        for r in &rows {
            let amount: f64 = r[1].parse().unwrap();
            let result: f64 = r[res_idx].parse().unwrap();
            let expected = if &r[v_idx] == "3" {
                saw_v3 = true;
                if &r[0] == "Argentina" {
                    amount * 3.0
                } else {
                    amount
                }
            } else if &r[0] == "Brazil" {
                amount * 2.0
            } else {
                amount
            };
            assert!((result - expected).abs() < 1e-9, "row {:?}", r);
        }
        assert!(saw_v3);
    }

    #[test]
    fn header_only_input_after_cycles_stops() {
        let tmp = tempfile::tempdir().unwrap();
        let input = write_input(tmp.path(), 30, None);
        let (state, tx) = setup(tmp.path(), 10_000, 8);
        start(&tx, input.clone()).blocking_recv().unwrap().unwrap();
        assert!(wait_for(&state, |s| s.cycles >= 2, Duration::from_secs(10)));
        let tmpf = tmp.path().join("input.tmp");
        std::fs::write(&tmpf, "country,amount,note\n").unwrap();
        std::fs::rename(&tmpf, &input).unwrap();
        assert!(wait_for(&state, |s| !s.running, Duration::from_secs(10)));
        let s = state.lock().unwrap();
        assert!(
            s.last_error.contains("zero data rows") || s.last_error.contains("headers changed"),
            "last_error={}",
            s.last_error
        );
        let cycles_at_stop = s.cycles;
        drop(s);
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(state.lock().unwrap().cycles, cycles_at_stop);
    }

    #[test]
    fn unique_output_dir_never_collides() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("data");
        let d1 = unique_output_dir(&base).unwrap();
        let d2 = unique_output_dir(&base).unwrap();
        assert_ne!(d1, d2);
        assert!(!slot_path(&d2, 0).exists());
        assert!(d2.starts_with(&base));
    }

    #[test]
    fn unique_output_dir_fails_on_file_base() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("not-a-dir");
        std::fs::write(&base, "x").unwrap();
        assert!(unique_output_dir(&base).is_err());
    }
}
