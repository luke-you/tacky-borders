use std::path::PathBuf;
use std::sync::OnceLock;

static CONFIG_HOME: OnceLock<PathBuf> = OnceLock::new();

pub fn initialize() -> &'static PathBuf {
    CONFIG_HOME.get_or_init(|| {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after UNIX epoch")
            .as_nanos();
        let config_home =
            std::env::temp_dir().join(format!("tacky-borders-test-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&config_home).expect("create isolated test configuration directory");
        std::fs::write(
            config_home.join("config.yaml"),
            include_str!("../src/resources/config.yaml"),
        )
        .expect("write repository-default test configuration");
        // Every test that can read APP_STATE or the config directory calls this initializer first.
        unsafe { std::env::set_var("TACKY_BORDERS_CONFIG_HOME", &config_home) };
        config_home
    })
}
