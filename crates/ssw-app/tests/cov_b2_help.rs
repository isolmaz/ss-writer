//! The weekly update check at the edges of its life: the state database that holds its schedule
//! cannot be opened, and the application is gone before the worker's answer arrives. The real
//! controller runs over a started application in a temporary profile without native windows;
//! the network answers from a script (nothing leaves the machine).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ssw_app::app::{AppCore, ShellHost, run};
use ssw_app::args::Args;
use ssw_app::startup::ProcessEnv;
use ssw_app::ui::controller::Controller;
use ssw_app::ui::scripted_services::ScriptedServices;
use ssw_io::net::{Http, ScriptedHttp, Url};
use ssw_io::paths::{AppPaths, Environment};
use ssw_io::update::RELEASES_URL;
use ssw_ui::services::{Clock, FakeClock, MemoryClipboard};
use ssw_ui::widgets::SharedClipboard;
use ssw_win::gfx::Driver;
use ssw_win::window::current_os_version;

const JSON_HEAD: &str = "HTTP/1.1 200 OK\r\nContent-Type: application/json";

/// Autosave would write unsaved edits behind the test's back.
const NO_AUTOSAVE: &str = r#"{"general":{"autosave":false}}"#;

/// Hands the controller of the started application to `body`, which owns it.
struct Host<B>(Option<B>);

impl<B: FnOnce(Rc<Controller>)> ShellHost for Host<B> {
    fn run(&mut self, core: AppCore, _args: &Args) -> i32 {
        let services = ScriptedServices::new();
        let clipboard: SharedClipboard = Rc::new(RefCell::new(MemoryClipboard::default()));
        let clock: Rc<dyn Clock> = Rc::new(FakeClock::default());
        let plan = core.plan.clone();
        let controller =
            Controller::new(core, services.dialogs(), services.shell(), clipboard, clock);
        controller.open_plan(&plan);
        (self.0.take().expect("the body runs once"))(controller);
        0
    }
}

/// Runs `body` over a started application whose settings file is `settings`.
fn with_controller(settings: &str, body: impl FnOnce(Rc<Controller>)) {
    let tmp = tempfile::tempdir().unwrap();
    let roaming = tmp.path().join("profile").join("roaming").join("SS Writer");
    std::fs::create_dir_all(&roaming).unwrap();
    std::fs::write(roaming.join("settings.json"), settings).unwrap();
    let mut args = Args::default();
    args.test.hidden = true;
    args.test.profile = Some(tmp.path().join("profile"));
    let process = ProcessEnv {
        os: current_os_version(),
        env: args.environment().unwrap(),
        os_language: "en-US".to_owned(),
        driver: Driver::Warp,
        install_crash_handler: false,
    };
    let code = run(
        &args,
        process,
        &mut *ScriptedServices::new().dialogs(),
        None,
        &mut Host(Some(body)),
    );
    assert_eq!(code, 0);
}

fn feed_url() -> String {
    Url::parse(RELEASES_URL).unwrap().to_string()
}

/// A network that answers the release feed with the release `tag`.
fn network_with(tag: &str) -> Arc<ScriptedHttp> {
    let http = Arc::new(ScriptedHttp::new());
    let body = format!(
        r#"{{"tag_name":"{tag}","html_url":"https://github.com/isolmaz/ss-writer/releases/tag/{tag}"}}"#
    );
    http.respond(&feed_url(), JSON_HEAD, body.as_bytes());
    http
}

#[test]
fn a_weekly_check_without_its_state_database_is_logged_and_asks_nobody() {
    with_controller(NO_AUTOSAVE, |controller| {
        let http = network_with("v99.0.0");
        controller.set_update_http(Arc::clone(&http) as Arc<dyn Http>);
        let log = controller.core().borrow().paths.logs_dir().join("ssw.log");

        // The schedule lives in the state database: the check is pointed at one whose folder is
        // a file, so it cannot be opened.
        let tmp = tempfile::tempdir().unwrap();
        let blocker = tmp.path().join("blocker");
        std::fs::write(&blocker, "x").unwrap();
        controller.core().borrow_mut().paths = AppPaths::resolve(&Environment {
            local_app_data: Some(blocker.clone()),
            app_data: Some(blocker),
            exe_dir: tmp.path().join("exe"),
        })
        .unwrap();

        controller.start_update_check();
        let deadline = Instant::now() + Duration::from_secs(30);
        while controller.core().borrow().workers.pending() > 0 {
            controller.core().borrow_mut().pump();
            assert!(Instant::now() < deadline, "the check never finished");
            std::thread::sleep(Duration::from_millis(5));
        }

        assert!(http.requests().is_empty(), "the network was never asked");
        assert_eq!(controller.available_update(), None);
        let text = std::fs::read_to_string(log).unwrap();
        assert!(text.contains("update check failed"), "{text}");
    });
}

#[test]
fn a_download_that_ends_after_the_application_is_gone_finds_nobody_to_tell() {
    with_controller(NO_AUTOSAVE, |controller| {
        let http = network_with("v99.0.0");
        controller.set_update_http(Arc::clone(&http) as Arc<dyn Http>);
        let workers = Rc::clone(&controller.core().borrow().workers);

        // The offer has to be known before the package can be asked for.
        controller.start_update_check();
        let deadline = Instant::now() + Duration::from_secs(30);
        while workers.pending() > 0 {
            controller.core().borrow_mut().pump();
            assert!(Instant::now() < deadline, "the check never finished");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(controller.available_update().is_some(), "an update is known");

        // The release carries no package, so the download fails without asking anybody; its
        // answer is delivered to a controller that is gone by then.
        controller.start_update_download();
        assert_eq!(workers.pending(), 1, "the download was handed to a worker");
        let gone = Rc::downgrade(&controller);
        drop(controller);
        assert!(
            gone.upgrade().is_none(),
            "nothing else keeps the controller"
        );

        let deadline = Instant::now() + Duration::from_secs(30);
        while workers.pending() > 0 {
            workers.poll();
            assert!(Instant::now() < deadline, "the download never finished");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(http.requests().len(), 1, "only the feed was asked");
    });
}

#[test]
fn an_answer_that_arrives_after_the_application_is_gone_finds_nobody_to_tell() {
    with_controller(NO_AUTOSAVE, |controller| {
        let http = network_with("v99.0.0");
        controller.set_update_http(Arc::clone(&http) as Arc<dyn Http>);
        let workers = Rc::clone(&controller.core().borrow().workers);
        let before = workers.pending();
        controller.start_update_check();
        assert_eq!(
            workers.pending(),
            before + 1,
            "the check was handed to a worker"
        );

        let gone = Rc::downgrade(&controller);
        drop(controller);
        assert!(
            gone.upgrade().is_none(),
            "nothing else keeps the controller"
        );

        // The worker still finishes; its callback is delivered to a controller that is gone.
        let deadline = Instant::now() + Duration::from_secs(30);
        while workers.pending() > 0 {
            workers.poll();
            assert!(Instant::now() < deadline, "the check never finished");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(http.requests().len(), 1, "the feed was asked once");
    });
}
