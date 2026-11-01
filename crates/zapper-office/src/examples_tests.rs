//! The `examples/` firm configs load, resolve on the office image, and have
//! all their workers' skills present. Moved here from the `zapper` binary when
//! the office left it (proposal 13, step 4): firm config is the office's own.

use zapper_policy::Config;

/// An example's directory: a config this build loads, whose profiles are all
/// on the one image the firms need (`zapper check` asks for every profile's
/// image), each firm's skills there, and a task with a subject.
fn example(name: &str) -> Config {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples")
        .join(name);
    // The examples' `skills_dir`/`agents_dir` are relative (`../../config/…`);
    // resolve them against the example's directory, as the daemon resolves
    // config paths against the config file's directory. Every example sits at
    // the same depth, so `set_config_dir` (set-once) is right for all of them.
    zapper_policy::set_config_dir(&dir);
    let c = Config::from_yaml(&std::fs::read_to_string(dir.join("zapper.yaml")).unwrap()).unwrap();
    for p in c.profiles.keys() {
        assert_eq!(c.resolve(p).unwrap().engine.image, "office", "{name}: {p}");
    }
    for firm in crate::firm::all(&c).values() {
        assert_eq!(crate::skills::missing(firm), Vec::<String>::new());
    }
    let task = std::fs::read_to_string(dir.join("task.md")).unwrap();
    assert!(!task.lines().next().unwrap().trim().is_empty());
    c
}

/// `examples/mini-firm`: Ada alone, who may contract a writer, named Ben from
/// the pool.
#[test]
fn the_mini_firm_example_loads() {
    let c = example("mini-firm");
    assert_eq!(c.profiles.len(), 1);
    let mini = &crate::firm::all(&c)["mini"];
    let founders: Vec<&str> = mini.workers.iter().map(|w| w.name.as_str()).collect();
    assert_eq!(founders, ["ada"]);
    assert_eq!(mini.roles["ceo"].hires, ["writer"]);
    assert!(mini.budget.seed > 0);
    assert_eq!(mini.names, ["ben"]);
}

/// `examples/shopping-list`: Ada alone, who may hire a product manager and
/// engineers; a git repository, which the profile leaves to grants; npm for
/// the engineers only.
#[test]
fn the_shopping_list_example_loads() {
    let c = example("shopping-list");
    let shop = &crate::firm::all(&c)["shop"];
    let founders: Vec<&str> = shop.workers.iter().map(|w| w.name.as_str()).collect();
    assert_eq!(founders, ["ada"]);
    assert_eq!(shop.roles["ceo"].hires, ["product-manager", "engineer"]);
    assert!(shop.budget.seed > 0);
    assert_eq!(shop.git_repos(), ["product"]);
    let hosts = |profile: &str| -> Vec<(String, zapper_policy::Action)> {
        c.resolve(profile)
            .unwrap()
            .layers
            .iter()
            .flat_map(|(_, l)| l.net.iter().map(|n| (n.host.clone(), n.action)))
            .collect()
    };
    let git = format!("git.{}", shop.domain("shop"));
    let npm = "registry.npmjs.org".to_string();
    for role in ["ceo", "product-manager", "engineer"] {
        let h = hosts(&shop.roles[role].profile);
        assert!(
            h.contains(&(git.clone(), zapper_policy::Action::Deny)),
            "{role}: {h:?}"
        );
        assert_eq!(
            h.iter().any(|(x, _)| *x == npm),
            role == "engineer",
            "{role}: {h:?}"
        );
    }
}

/// `examples/github-firm`: the shopping list company with its repositories on
/// GitHub, behind the firm's own git server.
#[test]
fn the_github_firm_example_loads() {
    let c = example("github-firm");
    let octo = &crate::firm::all(&c)["octo"];
    let founders: Vec<&str> = octo.workers.iter().map(|w| w.name.as_str()).collect();
    assert_eq!(founders, ["ada", "lin"]);
    assert_eq!(octo.roles["librarian"].agent.as_deref(), Some("librarian"));
    assert!(octo.git_internal());
    assert_eq!(octo.git_remote().unwrap().config, "acme.yaml");
    assert!(octo.git.as_ref().unwrap().dir.is_some());
    // Its knowledge base is kb, whose main is the firm's.
    assert_eq!(octo.knowledge_base().as_deref(), Some("kb"));
    assert!(octo.main_is_local("kb") && !octo.main_is_local("shopping-list"));
}

/// `examples/research-firm`: Ada alone, who may hire a research manager, who
/// may hire researchers, a prototype engineer, and a process lead; drive
/// spaces for the research and the board pack, a repository for prototypes.
#[test]
fn the_research_firm_example_loads() {
    let c = example("research-firm");
    let lab = &crate::firm::all(&c)["lab"];
    let founders: Vec<&str> = lab.workers.iter().map(|w| w.name.as_str()).collect();
    assert_eq!(founders, ["ada"]);
    assert_eq!(
        lab.roles["ceo"].hires,
        [
            "research-manager",
            "prototype-engineer",
            "process-improvement"
        ]
    );
    assert_eq!(lab.roles["research-manager"].hires, ["researcher"]);
    assert!(lab.budget.seed > 0);
    assert_eq!(lab.git_repos(), ["prototypes"]);
    for space in ["research", "board"] {
        assert!(lab.credentials.spaces.iter().any(|s| s == space), "{space}");
    }
}
