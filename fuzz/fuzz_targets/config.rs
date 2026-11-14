//! Arbitrary config files either fail cleanly or produce a config that
//! resolves its colors and survives a serialize/parse round trip.
#![no_main]

use libfuzzer_sys::fuzz_target;
use pakdeck::config::Config;

fuzz_target!(|data: &str| {
    let Ok(cfg) = toml::from_str::<Config>(data) else {
        return;
    };
    let _ = cfg.validate_themes();
    for (name, _) in pakdeck::theme::all(&cfg.themes) {
        let _ = pakdeck::theme::resolve(&cfg.theme, &cfg.themes, &name, true);
        let _ = pakdeck::theme::resolve(&cfg.theme, &cfg.themes, &name, false);
    }
    let text = toml::to_string(&cfg).expect("a parsed config must serialize");
    let back: Config = toml::from_str(&text).unwrap_or_else(|e| panic!("reparse failed: {e}\n{text}"));
    assert_eq!(back, cfg);
});
