use std::sync::Once;

static LOGGER_INIT: Once = Once::new();

pub fn init_android_logger() {
    LOGGER_INIT.call_once(|| {
        android_logger::init_once(
            android_logger::Config::default()
                .with_max_level(log::LevelFilter::Trace)
                .with_tag("TaskChampionJNI")
                .format(|f, record| {
                    write!(
                        f,
                        "{} [{}] {}",
                        record.level(),
                        record.module_path().unwrap_or_default(),
                        record.args()
                    )
                })
        );
        eprintln!("🦀 Android logger initialized for TaskChampion JNI");
    });
}