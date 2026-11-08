use std::fs::File;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use bread_tracker_server::{auth, db};
use serde_json::{json, Value};

struct ServerProcess {
    child: Option<Child>,
    base: String,
    log: PathBuf,
}

impl ServerProcess {
    async fn start(client: &reqwest::Client, db_path: &Path, log: PathBuf, max_items: i64) -> Self {
        let port = free_port();
        let out = File::create(&log).expect("create server log");
        let err = out.try_clone().expect("clone server log");
        let child = Command::new(env!("CARGO_BIN_EXE_bread-tracker-server"))
            .env("BREADTRACKER_DATABASE_PATH", db_path)
            .env("BREADTRACKER_LISTEN", format!("127.0.0.1:{port}"))
            .env("BREADTRACKER_MAX_ITEMS_PER_USER", max_items.to_string())
            .env("BREADTRACKER_REQUESTS_PER_MINUTE", "1000")
            .env("RUST_LOG", "warn")
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err))
            .spawn()
            .expect("spawn bread-tracker-server");
        let server = Self {
            child: Some(child),
            base: format!("http://127.0.0.1:{port}"),
            log,
        };
        server.wait_ready(client).await;
        server
    }

    async fn wait_ready(&self, client: &reqwest::Client) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if Instant::now() > deadline {
                panic!(
                    "server at {} did not become ready; log:\n{}",
                    self.base,
                    std::fs::read_to_string(&self.log).unwrap_or_default()
                );
            }
            let ready = client
                .get(format!("{}/healthz", self.base))
                .timeout(Duration::from_millis(500))
                .send()
                .await
                .is_ok_and(|r| r.status().is_success());
            if ready {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().expect("local address").port()
}

fn item(id: &str, name: &str, updated_at: i64) -> Value {
    json!({
        "id": id,
        "kind": "chore",
        "name": name,
        "weekdays": 127,
        "remind_time": null,
        "points": 10,
        "due_date": null,
        "sort": 0,
        "active": true,
        "updated_at": updated_at,
        "deleted_at": null,
    })
}

fn completion(item_id: &str, date: &str, at: i64) -> Value {
    json!({
        "item_id": item_id,
        "date": date,
        "completed_at": at,
        "updated_at": at,
        "deleted_at": null,
    })
}

async fn sync(client: &reqwest::Client, base: &str, token: &str, body: Value) -> u16 {
    client
        .post(format!("{base}/sync"))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .expect("POST /sync")
        .status()
        .as_u16()
}

fn now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

async fn devices(pool: &db::Db) -> (String, String) {
    let first_code = auth::create_pairing_code(pool).await.unwrap();
    let (_, first) = auth::redeem(pool, &first_code, "phone")
        .await
        .unwrap()
        .unwrap();
    let second_code = auth::create_pairing_code(pool).await.unwrap();
    let (_, second) = auth::redeem(pool, &second_code, "tablet")
        .await
        .unwrap()
        .unwrap();
    (first, second)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_sync_across_two_processes_stops_at_the_item_boundary() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("sync.db");
    let url = format!("sqlite://{}", db_path.display());
    let setup = db::connect(&url).await.unwrap();
    let (first_token, second_token) = devices(&setup).await;

    let client = reqwest::Client::new();
    let first =
        ServerProcess::start(&client, &db_path, directory.path().join("first.log"), 5).await;
    let second =
        ServerProcess::start(&client, &db_path, directory.path().join("second.log"), 5).await;

    // Eight concurrent one-item pushes across two OS processes, two tokens for one
    // owner. Only the first five may be admitted; the rest must be quota refusals,
    // never internal errors from a busy snapshot upgrade.
    let mut tasks = Vec::new();
    for index in 0..8 {
        let client = client.clone();
        let base = if index % 2 == 0 {
            first.base.clone()
        } else {
            second.base.clone()
        };
        let token = if index % 2 == 0 {
            first_token.clone()
        } else {
            second_token.clone()
        };
        let id = format!("00000000-0000-4000-8000-00000000000{index}");
        let body = json!({"items": [item(&id, &format!("item {index}"), now() + index)]});
        tasks.push(tokio::spawn(async move {
            sync(&client, &base, &token, body).await
        }));
    }
    let mut admitted = 0;
    let mut refused = 0;
    for task in tasks {
        match task.await.unwrap() {
            200 => admitted += 1,
            422 => refused += 1,
            other => panic!("unexpected POST /sync status {other}"),
        }
    }
    assert_eq!(
        (admitted, refused),
        (5, 3),
        "the fifth item is the boundary, not a 500"
    );

    let items: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM items")
        .fetch_one(&setup)
        .await
        .unwrap();
    assert_eq!(items, 5);
    let seqs: Vec<i64> = sqlx::query_scalar("SELECT seq FROM items ORDER BY seq")
        .fetch_all(&setup)
        .await
        .unwrap();
    assert_eq!(seqs, vec![1, 2, 3, 4, 5], "sequence numbers stay tight");
    let seq: i64 = sqlx::query_scalar("SELECT val FROM meta WHERE key = 'seq'")
        .fetch_one(&setup)
        .await
        .unwrap();
    assert_eq!(seq, 5);

    // A later push is still refused and changes nothing.
    let id = "00000000-0000-4000-8000-000000000099";
    assert_eq!(
        sync(
            &client,
            &first.base,
            &first_token,
            json!({"items": [item(id, "over the limit", now() + 100)]})
        )
        .await,
        422
    );
    let items: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM items")
        .fetch_one(&setup)
        .await
        .unwrap();
    assert_eq!(items, 5);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mixed_batch_that_crosses_the_boundary_rolls_back_as_one_transaction() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("mixed.db");
    let url = format!("sqlite://{}", db_path.display());
    let setup = db::connect(&url).await.unwrap();
    let (token, _) = devices(&setup).await;

    let client = reqwest::Client::new();
    let server =
        ServerProcess::start(&client, &db_path, directory.path().join("mixed.log"), 3).await;

    let first = "00000000-0000-4000-8000-000000000001";
    let second = "00000000-0000-4000-8000-000000000002";
    assert_eq!(
        sync(
            &client,
            &server.base,
            &token,
            json!({"items": [
                item(first, "first", now()),
                item(second, "second", now() + 1),
            ]})
        )
        .await,
        200
    );

    // Two new items plus completions do not fit in the one remaining slot. The
    // first item is written before the second is refused, so only a transaction
    // rollback can keep the account at two items, no completions and seq two.
    let third = "00000000-0000-4000-8000-000000000003";
    let fourth = "00000000-0000-4000-8000-000000000004";
    let at = now() + 10;
    assert_eq!(
        sync(
            &client,
            &server.base,
            &token,
            json!({
                "items": [item(third, "third", at), item(fourth, "fourth", at + 1)],
                "completions": [
                    completion(third, "2026-11-01", at),
                    completion(first, "2026-11-01", at + 1),
                ],
            })
        )
        .await,
        422
    );

    let items: Vec<String> = sqlx::query_scalar("SELECT id FROM items ORDER BY id")
        .fetch_all(&setup)
        .await
        .unwrap();
    assert_eq!(items, vec![first.to_string(), second.to_string()]);
    let completions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM completions")
        .fetch_one(&setup)
        .await
        .unwrap();
    assert_eq!(completions, 0, "the refused batch wrote no completions");
    let seq: i64 = sqlx::query_scalar("SELECT val FROM meta WHERE key = 'seq'")
        .fetch_one(&setup)
        .await
        .unwrap();
    assert_eq!(seq, 2, "the rolled-back sequence bump is gone");

    // The last slot is still usable with a mixed batch that fits.
    assert_eq!(
        sync(
            &client,
            &server.base,
            &token,
            json!({
                "items": [item(third, "third", at + 20)],
                "completions": [
                    completion(first, "2026-11-02", at + 21),
                    completion(second, "2026-11-02", at + 22),
                ],
            })
        )
        .await,
        200
    );
    let items: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM items")
        .fetch_one(&setup)
        .await
        .unwrap();
    assert_eq!(items, 3);
    let completions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM completions")
        .fetch_one(&setup)
        .await
        .unwrap();
    assert_eq!(completions, 2);
}
