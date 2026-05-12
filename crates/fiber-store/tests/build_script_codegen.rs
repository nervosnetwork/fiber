mod build_script {
    include!("../build.rs");

    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::sync::{Mutex, OnceLock};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    struct TestEnvGuard {
        current_dir: PathBuf,
        out_dir: Option<OsString>,
        manifest_dir: Option<OsString>,
    }

    impl TestEnvGuard {
        fn capture() -> Self {
            Self {
                current_dir: env::current_dir().expect("read current dir"),
                out_dir: env::var_os("OUT_DIR"),
                manifest_dir: env::var_os("CARGO_MANIFEST_DIR"),
            }
        }
    }

    impl Drop for TestEnvGuard {
        fn drop(&mut self) {
            env::set_current_dir(&self.current_dir).expect("restore current dir");

            match &self.out_dir {
                Some(value) => env::set_var("OUT_DIR", value),
                None => env::remove_var("OUT_DIR"),
            }

            match &self.manifest_dir {
                Some(value) => env::set_var("CARGO_MANIFEST_DIR", value),
                None => env::remove_var("CARGO_MANIFEST_DIR"),
            }
        }
    }

    fn unique_temp_dir() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time after unix epoch")
            .as_nanos();
        env::temp_dir().join(format!("fiber-store-build-script-test-{unique}"))
    }

    #[test]
    fn register_migrations_escapes_windows_manifest_dir_in_generated_rust() {
        let _guard = TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("lock build script test");
        let _env = TestEnvGuard::capture();

        let out_dir = unique_temp_dir();
        fs::create_dir_all(&out_dir).expect("create test OUT_DIR");

        env::set_current_dir(env!("CARGO_MANIFEST_DIR")).expect("switch to package root");
        env::set_var("OUT_DIR", &out_dir);
        env::set_var(
            "CARGO_MANIFEST_DIR",
            r"C:\agent\_work\1\s\crates\fiber-store",
        );

        main();

        let generated = fs::read_to_string(out_dir.join("register_migrations.rs"))
            .expect("read generated register_migrations.rs");

        assert!(generated.contains(r"C:\\agent\\_work\\1\\s\\crates\\fiber-store/src/migrations/"));
        assert!(!generated.contains(r"C:\agent\_work\1\s\crates\fiber-store/src/migrations/"));

        fs::remove_dir_all(out_dir).expect("remove test OUT_DIR");
    }
}
