//! Private tray host for ci/performance.py; never connect it to a desktop bus.
use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};

struct Watcher(Arc<Mutex<Vec<String>>>);

#[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
impl Watcher {
    fn register_status_notifier_item(&self, service: &str) {
        self.0.lock().unwrap().push(service.to_owned());
    }

    fn register_status_notifier_host(&self, _service: &str) {}

    #[zbus(property)]
    fn is_status_notifier_host_registered(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn registered_status_notifier_items(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }

    #[zbus(property)]
    fn protocol_version(&self) -> i32 {
        0
    }
}

fn main() -> anyhow::Result<()> {
    anyhow::ensure!(
        std::env::var("ESQUIE_PERFORMANCE_PRIVATE_BUS").as_deref() == Ok("1"),
        "only run through ci/performance.py on its private D-Bus session"
    );
    let items = Arc::new(Mutex::new(Vec::new()));
    let _connection = zbus::blocking::connection::Builder::session()?
        .name("org.kde.StatusNotifierWatcher")?
        .serve_at("/StatusNotifierWatcher", Watcher(items.clone()))?
        .build()?;
    println!("ready");
    std::io::stdout().flush()?;
    for line in std::io::stdin().lock().lines() {
        match line?.as_str() {
            "items" => println!("{}", serde_json::to_string(&*items.lock().unwrap())?),
            "quit" => break,
            other => anyhow::bail!("unknown command {other:?}"),
        }
        std::io::stdout().flush()?;
    }
    Ok(())
}
