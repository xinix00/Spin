use super::*;
use alloc::vec;

fn job(json: &str) -> Job {
    Job::from_json(json.as_bytes()).unwrap()
}
fn jobs() -> Vec<Job> {
    vec![
        job(
            r#"{"name":"easyshop-test-api","command":"x","env":{"SQL_PASSWORD":"s3cr3t-pass","PORT":"8080"},
                "tags":{"spin":"haas","spin-app":"easyshop/test","spin-allow":"logs, restart","hoplb-urlprefix":"easyshop-test.easyflor.nl/api/"}}"#,
        ),
        job(
            r#"{"name":"easyshop-test-db","command":"x","tags":{"spin":"haas","spin-app":"easyshop/test"}}"#,
        ),
        job(
            r#"{"name":"easyshop","command":"x","tags":{"spin":"haas","spin-app":"easyshop/prod"}}"#,
        ),
        job(r#"{"name":"other","command":"x","tags":{"spin":"bollenloods"}}"#),
        job(r#"{"name":"plain","command":"x"}"#),
    ]
}
fn task(job: &str, state: TaskState, restarts: i64, started: u64) -> Task {
    Task {
        id: "t".into(),
        job_name: job.into(),
        state,
        restart_count: restarts,
        started_at: Time(started),
        ..Default::default()
    }
}

#[test]
fn only_jobs_tagged_for_this_spin_are_visible() {
    let jobs = jobs();
    let e = exposure(&jobs[0], "haas").unwrap().unwrap();
    assert_eq!(
        e,
        Exposure {
            app: "easyshop/test".into(),
            service: "api".into(),
            logs: true,
            restart: true,
            update: false,
        }
    );
    let db = exposure(&jobs[1], "haas").unwrap().unwrap();
    assert_eq!(
        (db.service.as_str(), db.logs, db.restart),
        ("db", false, false)
    );
    // Een job zonder app-voorvoegsel houdt zijn eigen naam.
    assert_eq!(
        exposure(&jobs[2], "haas").unwrap().unwrap().service,
        "easyshop"
    );
    assert_eq!(exposure(&jobs[3], "haas").unwrap(), None);
    assert_eq!(exposure(&jobs[4], "haas").unwrap(), None);
    // Een lege filter toont niets, ook geen job met een lege tag.
    assert_eq!(exposure(&jobs[0], " ").unwrap(), None);
}

#[test]
fn rights_come_from_the_tags_only() {
    let jobs = jobs();
    let found = |app, service, right| find(&jobs, "haas", app, service, right).unwrap();
    assert_eq!(
        found("easyshop/test", "api", Right::Restart).unwrap().name,
        "easyshop-test-api"
    );
    assert!(found("easyshop/test", "db", Right::Logs).is_err());
    assert!(found("easyshop/test", "db", Right::Restart).is_err());
    assert!(found("easyshop/test", "db", Right::Status).is_ok());
    assert_eq!(
        found("easyshop/prod", "nope", Right::Status),
        Err("unknown service")
    );
    // Een job van een andere Spin is voor deze onvindbaar, ook met de juiste naam.
    assert_eq!(
        found("other", "other", Right::Status),
        Err("unknown service")
    );
}

#[test]
fn status_groups_services_per_app_with_their_worst_state() {
    let tasks = vec![AgentTasks {
        agent: "a1".into(),
        tasks: Some(vec![
            task("easyshop-test-api", TaskState::Running, 1, 10),
            task("easyshop-test-api", TaskState::Failed, 2, 20),
            task("easyshop", TaskState::Running, 0, 5),
        ]),
    }];
    let status = status(&jobs(), &tasks, "haas").unwrap();
    let apps = status.apps.as_slice();
    assert_eq!(
        apps.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(),
        ["easyshop/prod", "easyshop/test"]
    );
    let test = apps[1].services.as_slice();
    assert_eq!(
        test.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
        ["api", "db"]
    );
    assert_eq!(
        (test[0].state.as_str(), test[0].running, test[0].restarts),
        ("failed", 1, 3)
    );
    assert_eq!(test[0].allow.as_slice(), ["logs", "restart"]);
    assert_eq!(test[0].url, "easyshop-test.easyflor.nl/api/");
    assert_eq!(test[0].started_at.as_ref().unwrap().time().unwrap().0, 20);
    assert_eq!((test[1].state.as_str(), test[1].running), ("missing", 0));
    assert!(test[1].started_at.is_none());
}

#[test]
fn logs_are_masked_trimmed_and_capped() {
    let jobs = jobs();
    let mask = Mask::of(&jobs[0]).unwrap();
    let body = b": keepalive\n\ndata: login with s3cr3t-pass on 8080\n\ndata: two\ndata: lines\n\n";
    let mut lines = List::new();
    log_lines(body, "[stderr] ", &mask, &mut lines).unwrap();
    assert_eq!(
        lines.as_slice(),
        [
            "[stderr] login with ••• on 8080",
            "[stderr] two",
            "[stderr] lines"
        ]
    );
    let long = "é".repeat(LINE_MAX);
    let cut = mask.apply(&long).unwrap();
    assert!(cut.len() <= LINE_MAX + '…'.len_utf8() && cut.ends_with('…'));
    let mut many = List::new();
    for i in 0..LINES_MAX + 5 {
        many.push(text(format_args!("{i}")).unwrap()).unwrap();
    }
    keep_last(&mut many);
    assert_eq!(many.as_slice().len(), LINES_MAX);
    assert_eq!(many.as_slice()[0], "5");
}

#[test]
fn throttle_is_per_job() {
    let mut t = Throttle::new(RESTART_EVERY_NS);
    assert_eq!(t.admit("a", 0).unwrap(), Ok(()));
    assert_eq!(t.admit("b", 1).unwrap(), Ok(()));
    assert_eq!(t.admit("a", 1_000_000_000).unwrap(), Err(119));
    assert_eq!(t.admit("a", RESTART_EVERY_NS).unwrap(), Ok(()));
}

#[test]
fn update_swaps_only_the_url_within_the_same_source() {
    let mut j = job(
        r#"{"name":"lumen","driver":"hop","artifacts":[{"url":"s3://haas-software/artifacts/lumen/a.elf","match":{"node.arch":"arm64"},"auth":{"access_key":"k","secret_key":"s"}}]}"#,
    );
    update_artifact(&mut j, "s3://haas-software/artifacts/lumen/b.elf", "").unwrap();
    assert_eq!(
        j.artifacts[0].url,
        "s3://haas-software/artifacts/lumen/b.elf"
    );
    assert_eq!(j.artifacts[0].auth.get("access_key").unwrap(), "k");
    for bad in [
        "s3://other-bucket/x/b.elf",
        "https://haas-software/artifacts/b.elf",
        "s3://haas-software/b.elf",
        "s3://haas-software/artifacts/../../etc",
        "",
    ] {
        assert!(update_artifact(&mut j, bad, "").is_err(), "{bad}");
    }
    let mut gh = job(r#"{"name":"spin","driver":"hop","artifacts":[
            {"url":"https://github.com/xinix00/Spin/releases/download/v3.0.56/spin-server-arm64.elf","match":{"node.arch":"arm64"}},
            {"url":"https://github.com/xinix00/Spin/releases/download/v3.0.56/spin-server-riscv64.elf","match":{"node.arch":"riscv64"}}]}"#);
    assert!(
        update_artifact(
            &mut gh,
            "https://github.com/xinix00/Spin/releases/download/v3.0.57/spin-server-arm64.elf",
            ""
        )
        .is_err()
    );
    update_artifact(
        &mut gh,
        "https://github.com/xinix00/Spin/releases/download/v3.0.57/spin-server-arm64.elf",
        "arm64",
    )
    .unwrap();
    assert!(gh.artifacts[0].url.contains("v3.0.57") && gh.artifacts[1].url.contains("v3.0.56"));
    assert!(
        update_artifact(
            &mut gh,
            "https://github.com/someone-else/x/releases/download/a/b.elf",
            "arm64"
        )
        .is_err()
    );
    assert!(update_artifact(&mut gh, "https://github.com/xinix00/x.elf", "x86").is_err());
}
