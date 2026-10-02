//! Run with `cargo run --release -p mechanic-app --example services_bench --locked`.
//! Measures storage and transport only; no GUI, PTY, or user state is opened.
#[allow(dead_code)]
#[path = "../src/control.rs"]
mod control;
#[allow(dead_code)]
#[path = "../src/panes.rs"]
mod panes;
#[allow(dead_code)]
#[path = "../src/session.rs"]
mod session;

use std::collections::BTreeMap;
use std::hint::black_box;
use std::time::Instant;

fn measure(mut run: impl FnMut(), samples: usize) -> serde_json::Value {
    for _ in 0..10 {
        run();
    }
    let mut timings = Vec::with_capacity(samples);
    for _ in 0..samples {
        let start = Instant::now();
        run();
        timings.push(start.elapsed().as_nanos() as u64);
    }
    timings.sort_unstable();
    serde_json::json!({"samples": samples, "median_ns": timings[samples / 2],
        "p95_ns": timings[(samples - 1) * 95 / 100]})
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir_in("/tmp")?;
    let mut tree = panes::PaneTree::new(1);
    for index in 1..16 {
        tree.split_active(if index % 2 == 0 {
            panes::Axis::Horizontal
        } else {
            panes::Axis::Vertical
        })
        .unwrap();
    }
    let snapshot = session::SessionSnapshot::new(vec![session::WindowSnapshot {
        logical_size: [1024.0, 768.0],
        logical_position: Some([32.0, 48.0]),
        font_size: 14.0,
        panes: tree.snapshot(),
        directories: tree
            .pane_ids()
            .into_iter()
            .map(|id| (id, temporary.path().join("Straße-日本語")))
            .collect::<BTreeMap<_, _>>(),
    }]);
    let store = session::SessionStore::open(&temporary.path().join("state"))?;
    let encode = measure(
        || {
            black_box(serde_json::to_vec(black_box(&snapshot)).unwrap());
        },
        2000,
    );
    let save = measure(
        || {
            store.save(snapshot.clone()).unwrap();
            store.flush().unwrap();
        },
        100,
    );
    assert_eq!(store.load(), Some(snapshot));

    let server = control::ControlServer::start_in(temporary.path().join("ctl"), |event| {
        event
            .reply
            .try_send(control::Response::new(
                event.request.instance_id.unwrap(),
                control::ResponseResult::Panes { panes: Vec::new() },
            ))
            .map_err(|_| ())
    })?;
    let request =
        control::Request::new(Some(server.instance_id().into()), control::Operation::ListPanes);
    let roundtrip = measure(
        || {
            let response = control::request(server.socket_path(), &request).unwrap();
            assert!(matches!(response.result, control::ResponseResult::Panes { .. }));
            black_box(response);
        },
        500,
    );
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "scope": "16-pane JSON encode; atomic durable save including worker queue; authenticated socket transport with immediate empty-list callback. Excludes GUI dispatch, PTY and output extraction.",
            "snapshot_encode": encode, "snapshot_save_flush": save, "socket_roundtrip": roundtrip
        }))?
    );
    Ok(())
}
