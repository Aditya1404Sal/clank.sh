//! The `/var/log` sinks for clank's families: what an outbound family call writes there. The core
//! half — shell.log's start/end pair and ops.log's destructive record — lives in
//! `bash::session::tests::logging`.
//!
//! Fixtures live in the parent module ([`super`]).

use super::*;

/// An `ask` LLM turn is recorded in http.log (via the `LoggingAskProvider` wrapper).
#[test]
fn http_log_records_the_llm_turn() {
    on_rt(async {
        let cap = LogCapture::new("http");
        let mut session = Session::new().await.unwrap();
        session.install_clank();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        session.set_ask_provider(Box::new(FakeProvider::reply("reply", seen)));
        session.eval_line(r#"sudo ask "hello""#).await;
        let log = cap.read(crate::logging::LogFile::Http);
        assert!(
            log.contains("kind=llm"),
            "http.log should record the LLM call, got:\n{log}"
        );
        assert!(log.contains("status=ok"), "got:\n{log}");
    });
}
