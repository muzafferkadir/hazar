use crate::bridge::QueueEntry;
use hazar_localapi::{GrabRequest, Settings};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::PathBuf, sync::Mutex};

#[derive(Serialize, Deserialize)]
pub struct SavedState {
    pub settings: Settings,
    pub entries: Vec<QueueEntry>,
    pub requests: HashMap<String, GrabRequest>,
}

pub struct Store {
    dir: PathBuf,
    lock: Mutex<()>,
}
impl Store {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            lock: Mutex::new(()),
        }
    }
    pub fn load(&self) -> Option<SavedState> {
        for name in ["downloads.json", "downloads.json.bak"] {
            if let Ok(raw) = std::fs::read(self.dir.join(name)) {
                match serde_json::from_slice(&raw) { Ok(saved) => return Some(saved), Err(error) => eprintln!("hazar: saved state unreadable: {error}") }
            }
        }
        None
    }
    pub fn save(
        &self,
        settings: &Settings,
        entries: &[QueueEntry],
        requests: &HashMap<String, GrabRequest>,
    ) -> Result<(), String> {
        let _guard = self.lock.lock().map_err(|_| "state lock")?;
        let mut requests = requests.clone();
        for request in requests.values_mut() {
            request.cookie = None;
            request.headers.retain(|(key, _)| {
                matches!(
                    key.to_ascii_lowercase().as_str(),
                    "referer" | "accept" | "origin"
                )
            });
        }
        let data = serde_json::to_vec(&SavedState {
            settings: settings.clone(),
            entries: entries.to_vec(),
            requests,
        })
        .map_err(|e| e.to_string())?;
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        let tmp = self.dir.join("downloads.json.tmp");
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&tmp).map_err(|e| e.to_string())?;
        use std::io::Write;
        file.write_all(&data)
            .and_then(|_| file.sync_all())
            .map_err(|e| e.to_string())?;
        drop(file);
        let dest = self.dir.join("downloads.json");
        #[cfg(windows)]
        if dest.exists() {
            std::fs::copy(&dest, self.dir.join("downloads.json.bak")).map_err(|e| e.to_string())?;
            std::fs::remove_file(&dest).map_err(|e| e.to_string())?;
        }
        std::fs::rename(tmp, dest).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn settings_and_requests_survive_restart_without_cookies() {
        let dir = std::env::temp_dir().join(format!("hazar-store-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        let store = Store::new(dir.clone());
        let request: GrabRequest = serde_json::from_value(serde_json::json!({"url":"https://example.com/a", "kind":"file", "cookie":"sid=secret", "headers":[["Authorization","secret"],["Referer","https://example.com/"]]})).unwrap();
        let mut settings = Settings::default(); settings.max_concurrent_downloads = 2;
        store.save(&settings, &[], &HashMap::from([("job".into(), request)])).unwrap();
        let saved = Store::new(dir.clone()).load().unwrap();
        assert_eq!(saved.settings.max_concurrent_downloads, 2);
        assert!(saved.requests["job"].cookie.is_none());
        assert!(!std::fs::read_to_string(dir.join("downloads.json")).unwrap().contains("secret"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
