//! Headless Chromium pool used for JS-only or bot-protected SERPs.
//!
//! One browser process is launched lazily on first use and shared by all
//! requests; concurrency is bounded by a tab semaphore. Images and fonts are
//! disabled to keep CPU/RAM low. A crashed browser is relaunched on the next
//! request.

use url::Url;

use crate::config::BrowserSettings;

#[cfg(feature = "browser")]
pub use imp::BrowserPool;

#[cfg(not(feature = "browser"))]
pub use stub::BrowserPool;

/// Shared state reported on `/ready`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BrowserStatus {
    /// Not compiled in or disabled by config.
    Disabled,
    /// Enabled, not launched yet (launches on first use).
    Idle,
    Running,
}

#[cfg(feature = "browser")]
mod imp {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };

    use chromiumoxide::{
        Page,
        browser::{Browser, BrowserConfig},
        cdp::browser_protocol::network::{BlockPattern, SetBlockedUrLsParams},
    };
    use futures::StreamExt;
    use tokio::{
        sync::{Mutex, Semaphore},
        time::{Instant, sleep, timeout},
    };

    use super::*;

    /// URL patterns (URLPattern syntax) that are never needed to read a SERP.
    const BLOCKED_PATTERNS: &[&str] = &[
        "*://*/*.css*",
        "*://*/*.woff*",
        "*://*/*.ttf*",
        "*://*/*.mp4*",
        "*://*/*.webm*",
        "*://*.doubleclick.net/*",
        "*://*.googlesyndication.com/*",
        "*://*.google-analytics.com/*",
        "*://*.googletagmanager.com/*",
        "*://*.scorecardresearch.com/*",
    ];

    struct Instance {
        browser: Browser,
        alive: Arc<AtomicBool>,
    }

    pub struct BrowserPool {
        settings: BrowserSettings,
        proxy: Option<String>,
        user_agent: String,
        instance: Mutex<Option<Arc<Instance>>>,
        tabs: Semaphore,
    }

    impl BrowserPool {
        pub fn new(settings: BrowserSettings, proxy: Option<String>, user_agent: String) -> Self {
            let tabs = Semaphore::new(settings.max_tabs);
            Self {
                settings,
                proxy,
                user_agent,
                instance: Mutex::new(None),
                tabs,
            }
        }

        pub fn is_enabled(&self) -> bool {
            self.settings.enabled
        }

        pub fn status(&self) -> BrowserStatus {
            if !self.settings.enabled {
                return BrowserStatus::Disabled;
            }
            match self.instance.try_lock() {
                Ok(guard) => match guard.as_ref() {
                    Some(i) if i.alive.load(Ordering::Relaxed) => BrowserStatus::Running,
                    _ => BrowserStatus::Idle,
                },
                // Locked = launching or in use.
                Err(_) => BrowserStatus::Running,
            }
        }

        async fn launch(&self) -> Result<Instance, String> {
            let data_dir = std::env::temp_dir().join(format!("agentic-web-search-chrome-{}", std::process::id()));
            let mut b = BrowserConfig::builder()
                .new_headless_mode()
                .no_sandbox()
                .user_data_dir(data_dir)
                .launch_timeout(Duration::from_secs(20))
                .request_timeout(self.settings.page_timeout)
                .args([
                    "--disable-gpu",
                    "--disable-dev-shm-usage",
                    "--disable-extensions",
                    "--disable-background-networking",
                    "--disable-background-timer-throttling",
                    "--disable-default-apps",
                    "--disable-sync",
                    "--disable-translate",
                    "--disable-features=Translate,MediaRouter,OptimizationHints",
                    "--disable-remote-fonts",
                    "--blink-settings=imagesEnabled=false",
                    "--mute-audio",
                    "--no-first-run",
                    "--lang=en-US",
                ]);
            if let Some(exe) = &self.settings.executable {
                b = b.chrome_executable(exe);
            }
            if let Some(proxy) = &self.proxy {
                b = b.arg(format!("--proxy-server={proxy}"));
            }
            let config = b.build()?;
            let (browser, mut handler) = Browser::launch(config).await.map_err(|e| e.to_string())?;
            let alive = Arc::new(AtomicBool::new(true));
            let flag = alive.clone();
            tokio::spawn(async move {
                // Individual handler errors are usually unknown CDP events; keep polling
                // until the connection closes.
                while handler.next().await.is_some() {}
                flag.store(false, Ordering::Relaxed);
                tracing::warn!("headless browser connection closed");
            });
            tracing::info!("headless browser launched");
            Ok(Instance { browser, alive })
        }

        async fn instance(&self) -> Result<Arc<Instance>, String> {
            let mut guard = self.instance.lock().await;
            if let Some(inst) = guard.as_ref().filter(|i| i.alive.load(Ordering::Relaxed)) {
                return Ok(inst.clone());
            }
            let inst = Arc::new(self.launch().await?);
            *guard = Some(inst.clone());
            Ok(inst)
        }

        /// Render `url` and return the DOM once `ready_selector` appears
        /// (or the page timeout elapses).
        pub async fn render(&self, url: &Url, ready_selector: &str) -> Result<String, String> {
            if !self.settings.enabled {
                return Err("browser disabled (BROWSER_ENABLED=false)".into());
            }
            let _permit = self.tabs.acquire().await.map_err(|e| e.to_string())?;
            let inst = self.instance().await?;
            let budget = self.settings.page_timeout;
            let page = timeout(budget, inst.browser.new_page("about:blank"))
                .await
                .map_err(|_| "timed out opening tab".to_owned())?
                .map_err(|e| {
                    inst.alive.store(false, Ordering::Relaxed);
                    e.to_string()
                })?;
            let result = timeout(budget, self.drive(&page, url, ready_selector))
                .await
                .unwrap_or_else(|_| Err("page render timed out".into()));
            // Always close the tab, even on error/timeout, to free renderer memory.
            let _ = page.close().await;
            result
        }

        async fn drive(&self, page: &Page, url: &Url, ready_selector: &str) -> Result<String, String> {
            page.enable_stealth_mode_with_agent(&self.user_agent)
                .await
                .map_err(|e| e.to_string())?;
            // Skip bytes the parser never needs (images are already off via blink-settings).
            let _ = page
                .execute(
                    SetBlockedUrLsParams::builder()
                        .url_patterns(BLOCKED_PATTERNS.iter().map(|p| BlockPattern::new(*p, true)))
                        .build(),
                )
                .await;
            // Navigate via JS instead of `goto`: `goto` waits for the full `load`
            // event (ads, trackers, ...), while results are usually in the DOM long
            // before that. We return as soon as the engine's ready selector matches.
            let target = serde_json::to_string(url.as_str()).map_err(|e| e.to_string())?;
            page.evaluate(format!("window.location.href = {target}"))
                .await
                .map_err(|e| e.to_string())?;
            let wait_until = Instant::now() + self.settings.page_timeout.saturating_sub(Duration::from_secs(1));
            loop {
                sleep(Duration::from_millis(150)).await;
                let on_target = page.url().await.ok().flatten().is_some_and(|u| u != "about:blank");
                if on_target && page.find_element(ready_selector).await.is_ok() {
                    break;
                }
                if Instant::now() >= wait_until {
                    break;
                }
            }
            page.content().await.map_err(|e| e.to_string())
        }

        pub async fn shutdown(&self) {
            if let Some(inst) = self.instance.lock().await.take() {
                // If a render still holds a reference, dropping the last one kills the child (kill_on_drop).
                if let Ok(mut inst) = Arc::try_unwrap(inst) {
                    let _ = inst.browser.close().await;
                    let _ = inst.browser.wait().await;
                }
            }
        }
    }
}

#[cfg(not(feature = "browser"))]
mod stub {
    use super::*;

    pub struct BrowserPool;

    impl BrowserPool {
        pub fn new(_settings: BrowserSettings, _proxy: Option<String>, _user_agent: String) -> Self {
            BrowserPool
        }

        pub fn is_enabled(&self) -> bool {
            false
        }

        pub fn status(&self) -> BrowserStatus {
            BrowserStatus::Disabled
        }

        pub async fn render(&self, _url: &Url, _ready_selector: &str) -> Result<String, String> {
            Err("binary built without the `browser` feature".into())
        }

        pub async fn shutdown(&self) {}
    }
}
