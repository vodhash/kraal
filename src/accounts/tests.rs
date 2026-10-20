use crate::identity::new_uuid;

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;

use super::store::HEADER;
use super::usage::usage_file;
use super::*;
use crate::paths;

#[test]
fn launch_command_preserves_arguments_and_environment_overrides() {
    use std::ffi::OsStr;

    let mut launch = Launch {
        argv: vec!["agent".into()],
        env: vec![("ACCOUNT".into(), "first".into()), ("ACCOUNT".into(), "last".into())],
        env_remove: vec!["API_KEY".into(), "ACCOUNT".into()],
    };
    let cmd = launch.command();
    assert_eq!(cmd.get_program(), "agent");
    assert_eq!(cmd.get_args().count(), 0);
    assert_eq!(
        cmd.get_envs().collect::<Vec<_>>(),
        [(OsStr::new("ACCOUNT"), Some(OsStr::new("last"))), (OsStr::new("API_KEY"), None)]
    );
    assert_eq!(cmd.get_current_dir(), None);

    launch.argv.extend(["two words".into(), "".into(), "$HOME;*".into()]);
    let cmd = launch.command();
    assert_eq!(cmd.get_program(), "agent");
    assert_eq!(cmd.get_args().collect::<Vec<_>>(), ["two words", "", "$HOME;*"]);
}

#[test]
fn duplicate_agent_arguments_keep_options_and_prompts_without_selecting_old_conversations() {
    let words = |s: &str| s.split_whitespace().map(str::to_string).collect::<Vec<_>>();
    for (agent, before, after) in [
        ("claude", "--session-id old --model opus", "--model opus"),
        ("claude", "--model opus --resume old --continue --fork-session prompt", "--model opus prompt"),
        ("claude", "--resume=old --session-id=old -c -r old -p prompt", "-p prompt"),
        ("claude", "--system-prompt --continue --model opus", "--system-prompt --continue --model opus"),
        ("claude", "--model=opus -- --resume old", "--model=opus -- --resume old"),
        ("codex", "resume old -m model prompt", "-m model prompt"),
        ("codex", "-m model fork --last --all prompt", "-m model prompt"),
        ("codex", "resume --all old -c mode=local prompt", "-c mode=local prompt"),
        ("codex", "resume -- old prompt", "-- prompt"),
        ("codex", "-m resume prompt", "-m resume prompt"),
        ("codex", "exec prompt", "exec prompt"),
        ("codex", "exec resume old --json prompt", "exec --json prompt"),
        ("codex", "e resume --last prompt", "e prompt"),
    ] {
        assert_eq!(duplicate_args(agent_kind(agent).unwrap(), &words(before)), words(after), "{agent} {before}");
    }
}

#[test]
fn duplicate_codex_exec_keeps_option_values_on_either_side_of_resume() {
    let kind = agent_kind("codex").unwrap();
    for option in ["-o", "--output-last-message", "--output-schema", "--color"] {
        for value in ["report.txt", "resume", "review"] {
            for args in [
                vec!["exec", "resume", option, value, "SESSION", "prompt"],
                vec!["exec", option, value, "resume", "SESSION", "prompt"],
            ] {
                let args: Vec<_> = args.into_iter().map(str::to_string).collect();
                assert_eq!(duplicate_args(kind, &args), ["exec", option, value, "prompt"], "{args:?}");
            }
            let attached = format!("{option}={value}");
            for args in [
                vec!["exec", "resume", &attached, "SESSION", "prompt"],
                vec!["exec", &attached, "resume", "SESSION", "prompt"],
            ] {
                let args: Vec<_> = args.into_iter().map(str::to_string).collect();
                assert_eq!(duplicate_args(kind, &args), ["exec", &attached, "prompt"], "{args:?}");
            }
        }
    }
}

#[test]
fn duplicate_claude_removes_optional_and_attached_conversation_selectors() {
    let kind = agent_kind("claude").unwrap();
    for selector in ["--from-pr", "--teleport"] {
        for selection in [vec![selector], vec![selector, "123"], vec![selector, "SESSION"]] {
            let args: Vec<_> = selection.into_iter().chain(["--model", "opus", "prompt"]).map(str::to_string).collect();
            assert_eq!(duplicate_args(kind, &args), ["--model", "opus", "prompt"], "{args:?}");
        }
        let args = [format!("{selector}=SESSION"), "prompt".into()];
        assert_eq!(duplicate_args(kind, &args), ["prompt"]);
        let args = [selector.into(), "--".into(), "prompt".into()];
        assert_eq!(duplicate_args(kind, &args), ["--", "prompt"]);
        let args = ["--system-prompt".into(), selector.into(), "--".into(), selector.into()];
        assert_eq!(duplicate_args(kind, &args), args, "option values and literal prompts stay");
    }
}

#[test]
fn duplicate_codex_keeps_subcommand_names_as_prompts_and_trailing_options_as_options() {
    let kind = agent_kind("codex").unwrap();
    for prefix in [vec![], vec!["exec"], vec!["e"]] {
        for selector in ["resume", "fork"] {
            for prompt in ["review", "resume", "fork", "exec", "help"] {
                for selection in ["SESSION", "--last"] {
                    let args: Vec<_> = prefix
                        .iter()
                        .copied()
                        .chain([selector, selection, prompt, "--model", "model"])
                        .map(str::to_string)
                        .collect();
                    let expected: Vec<_> =
                        prefix.iter().copied().chain(["--model", "model", "--", prompt]).map(str::to_string).collect();
                    assert_eq!(duplicate_args(kind, &args), expected, "{args:?}");
                }
            }
        }
    }
    let args = ["exec", "resume", "SESSION", "--", "review", "--model", "literal"].map(str::to_string);
    assert_eq!(duplicate_args(kind, &args), ["exec", "--", "review", "--model", "literal"]);
}

#[test]
fn linked_files_rewrite_to_one_entry_per_account_with_all_references_and_unknown_fields() {
    let dir = std::env::temp_dir().join(format!("kraal-compact-{}", new_uuid()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("accounts.toml");
    let source = "# My accounts.\n\
[[account]]\nid = 'work'\nname = 'patron.bzh'\nagent = 'claude'\nhome = '/patron'\n\
[[account]]\nid = 'gmail'\nname = 'gmail'\nagent = 'claude'\nhome = '/gmail'\n\
# Keep the old home.\n\
[[account]]\nid = 'old-claude'\nname = 'default'\nagent = 'claude'\nhome = '~/.claude'\nlinked_to = 'default-claude'\nnote = 'mine'\nfuture = 7\n\
[[account]]\nid = 'default-claude'\nname = 'Personal'\nagent = 'claude'\nhome = '~/.claude'\nlinked_to = 'work'\n\
[[account]]\nid = 'old-codex'\nname = 'default'\nagent = 'codex'\nhome = '~/.codex'\nlinked_to = 'default-codex'\n\
[[account]]\nid = 'default-codex'\nname = 'Personal'\nagent = 'codex'\nhome = '~/.codex'\n\
[[rule]]\nworkspace = 'w'\nagent = 'claude'\naccounts = ['default']\n";
    std::fs::write(&path, source).unwrap();
    Accounts::update_at(&path, |_| Ok(())).unwrap();
    let saved = std::fs::read_to_string(&path).unwrap();
    assert_eq!(saved.matches("[[account]]").count(), 3, "{saved}");
    assert!(!saved.contains("linked_to"), "{saved}");
    for kept in ["# My accounts.", "# Keep the old home.", "note = 'mine'", "future = 7"] {
        assert!(saved.contains(kept), "{saved}");
    }
    let all = Accounts::load_from(&path).unwrap();
    assert_eq!(all.all().len(), 3);
    for id in ["old-claude", "default-claude", "work"] {
        assert_eq!(all.by_id(id).unwrap().name, "patron.bzh", "{saved}");
    }
    assert_eq!(all.get("claude", "default").unwrap().id, "work");
    assert_eq!(all.get("claude", "Personal").unwrap().id, "work");
    assert_eq!(all.by_home("claude", &paths::expand_tilde("~/.claude")).unwrap().id, "work");
    assert_eq!(all.by_id("old-codex").unwrap().name, "Personal");
    assert_eq!(all.choose("claude", Some("w"), None, &UsageMap::default(), 0).unwrap().id, "work");
    let env = vec![("CLAUDE_CONFIG_DIR".into(), paths::expand_tilde("~/.claude").display().to_string())];
    let launched = all
        .pane_account(&crate::pane::PaneSpec { account: Some("claude:default".into()), env, ..Default::default() })
        .unwrap();
    assert_eq!(launched.id, "work");
    assert!(launched.same_home(&Accounts::standard("claude").unwrap()));

    Accounts::update_at(&path, |all| all.save()).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), saved, "rewriting is idempotent");
    Accounts::update_at(&path, |all| {
        all.limits.threshold = 75;
        Ok(())
    })
    .unwrap();
    let changed = std::fs::read_to_string(&path).unwrap();
    assert!(changed.contains("# Keep the old home.") && changed.contains("future = 7"));
    assert_eq!(changed.matches("[[account]]").count(), 3);
    Accounts::update_at(&path, |all| {
        assert!(all.remove("claude", "patron.bzh"));
        Ok(())
    })
    .unwrap();
    let all = Accounts::load_from(&path).unwrap();
    assert!(all.by_id("default-claude").is_none());
    assert!(all.unlisted().iter().all(|a| a.agent != "claude"));
    assert_eq!(all.all().len(), 2);
    assert_eq!(std::fs::read_to_string(&path).unwrap().matches("[[account]]").count(), 2);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn compaction_keeps_nested_unknown_fields_with_their_reference() {
    let dir = std::env::temp_dir().join(format!("kraal-compact-nested-{}", new_uuid()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("accounts.toml");
    let source = "[[account]]\nid = 'work'\nname = 'work'\nagent = 'claude'\nhome = '/work'\n\
[[account]]\nid = 'personal'\nname = 'personal'\nagent = 'claude'\nhome = '/personal'\n\
# Keep this reference.\n\
[[account]]\nid = 'old'\nname = 'old'\nagent = 'claude'\nhome = '/old'\nlinked_to = 'work'\n\
[account.future] # Keep this table.\nvalue = 7 # Keep this value.\n\
[account.future.nested]\nlabel = 'nested'\n\
[[account.future.entries]] # Keep this entry.\nname = 'first'\n\
[account.future.entries.options]\nenabled = true\n\
[[account.future.entries]]\nname = 'second'\n";
    std::fs::write(&path, source).unwrap();
    let original: toml::Value = toml::from_str(source).unwrap();
    let future = &original["account"][2]["future"];
    for forgotten in [false, true] {
        Accounts::update_at(&path, |all| {
            if forgotten {
                assert!(all.remove("claude", "work"));
            }
            Ok(())
        })
        .unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        let doc: toml::Value = toml::from_str(&saved).unwrap();
        let references = if forgotten {
            doc["forgotten"].as_array().unwrap()
        } else {
            doc["account"][0]["references"].as_array().unwrap()
        };
        let old = references.iter().find(|a| a["id"].as_str() == Some("old")).unwrap();
        assert_eq!(&old["future"], future, "{saved}");
        let personal =
            doc["account"].as_array().unwrap().iter().find(|a| a["id"].as_str() == Some("personal")).unwrap();
        assert!(personal.get("references").is_none(), "{saved}");
        for comment in ["# Keep this reference.", "# Keep this table.", "# Keep this value.", "# Keep this entry."] {
            assert!(saved.contains(comment), "{saved}");
        }
        let all = Accounts::load_from(&path).unwrap();
        assert_eq!(all.all().len(), if forgotten { 1 } else { 2 });
        assert_eq!(all.by_id("old").map(|a| a.id), (!forgotten).then(|| "work".to_string()));
        Accounts::update_at(&path, |_| Ok(())).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), saved, "rewriting is idempotent");
        Accounts::update_at(&path, |all| {
            all.limits.threshold = 75;
            Ok(())
        })
        .unwrap();
        assert_eq!(Accounts::load_from(&path).unwrap().limits.threshold, 75);
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn inline_references_and_empty_lists_survive_rewriting() {
    let dir = std::env::temp_dir().join(format!("kraal-inline-references-{}", new_uuid()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("accounts.toml");
    for source in [
        "account = [{id = 'work', name = 'work', agent = 'claude', home = '/work', references = [{id = 'old', name = 'old', agent = 'claude', home = '/old', future = 7}]}]\n",
        "account = []\nforgotten = []\n",
        "[[account]]\nid = 'work'\nname = 'work'\nagent = 'claude'\nhome = '/work'\nreferences = []\n",
    ] {
        std::fs::write(&path, source).unwrap();
        Accounts::update_at(&path, |all| {
            all.limits.threshold = 75;
            Ok(())
        })
        .unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        let all = Accounts::load_from(&path).unwrap();
        assert_eq!(all.limits.threshold, 75);
        if source.contains("future") {
            assert!(saved.contains("future = 7"), "{saved}");
            assert_eq!(all.get("claude", "old").unwrap().id, "work");
            assert_eq!(all.all().len(), 1);
        }
        Accounts::update_at(&path, |all| all.save()).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), saved);
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn verified_existing_sign_ins_are_ordinary_renameable_accounts() {
    for kind in agents() {
        let mut all = Accounts::empty();
        let original = all.get(kind.name, "default").unwrap();
        assert!(all.all().is_empty());
        all.adopt_checked(&original, None).unwrap();
        assert_eq!(all.for_agent(kind.name).len(), 1);
        let saved: toml::Value = toml::from_str(&all.text().unwrap()).unwrap();
        assert_eq!(saved["account"].as_array().unwrap().len(), 1);
        let adopted = all.get(kind.name, "Personal").unwrap();
        assert_eq!(adopted.id, original.id);
        assert_eq!(adopted.home, original.home);
        assert!(!adopted.sign_in_required);
        let legacy = all.accounts.iter().find(|a| a.agent == kind.name && a.name == "default").unwrap();
        assert_eq!(legacy.home, original.home);
        assert_eq!(legacy.linked_to.as_deref(), Some(adopted.id.as_str()));
        assert_ne!(legacy.id, adopted.id);
        all.rename(kind.name, "default", "Mine").unwrap();
        assert_eq!(all.get(kind.name, "default").unwrap().name, "Mine");
        assert_eq!(all.by_id(&original.id).unwrap().name, "Mine");
        assert!(all.unlisted().iter().all(|a| a.agent != kind.name));
        assert!(all.remove(kind.name, "Mine"));
        assert!(all.for_agent(kind.name).is_empty());
        assert!(all.by_id(&original.id).is_none());
        assert!(all.unlisted().iter().all(|a| a.agent != kind.name), "forgetting is permanent");
    }
}

#[test]
fn public_identity_proposes_a_name_but_a_matching_label_is_not_a_checked_identity() {
    let mut all = Accounts::empty();
    let existing = all.get("claude", "default").unwrap();
    let email = "me+personal@example.com";
    all.adopt_checked(&existing, Some(email)).unwrap();
    assert_eq!(all.get("claude", "default").unwrap().name, email);
    all.rename("claude", email, "Personal").unwrap();
    assert_eq!(all.proposed_name("claude", None), "");
    assert_eq!(all.proposed_name("codex", Some("bad\nname@example.com")), "");
    let mut other = Accounts::empty();
    other.add(acct("claude", email, "/a-different-home")).unwrap();
    other.adopt_checked(&existing, Some(email)).unwrap();
    assert_eq!(other.all().len(), 1);
    assert_eq!(other.reviews().len(), 1, "an account label is not its provider identity");
    let pending = other.reviews().pop().unwrap();
    assert_eq!(pending.public_identity.as_deref(), Some(email));
    other.review(&pending, None).unwrap();
    assert_eq!(other.all().len(), 2);
    assert_eq!(other.get("claude", "default").unwrap().name, "me+personal@example.com2");
    assert!(other.reviews().is_empty());
}

#[test]
fn a_rule_for_a_pending_or_legacy_claude_account_leaves_codex_unrestricted() {
    let mut all = Accounts::empty();
    let codex = all.add(acct("codex", "work", "/codex-work")).unwrap();
    all.add(acct("claude", "work", "/claude-work")).unwrap();
    let existing = all.get("claude", "default").unwrap();
    all.rules.push(Rule {
        workspace: Some("w".into()),
        path: None,
        agent: None,
        accounts: vec!["claude:default".into()],
        account_ids: None,
    });
    let chosen = |all: &Accounts| all.choose("codex", Some("w"), None, &UsageMap::default(), 0);
    assert_eq!(chosen(&all), Some(codex.clone()), "the implicit legacy account resolves");
    all.adopt_checked(&existing, None).unwrap();
    assert_eq!(all.reviews().len(), 1);
    assert_eq!(chosen(&all), Some(codex.clone()), "a pending review still resolves by name");
    all.bind_rules();
    assert_eq!(chosen(&all), Some(codex.clone()), "a pending review still resolves by identity");
    all.remove("claude", "default");
    assert_eq!(chosen(&all), None, "a truly dangling rule still forbids every account");
}

#[test]
fn review_links_old_references_without_moving_a_running_or_saved_panes_home() {
    let mut all = Accounts::empty();
    let original = all.get("claude", "default").unwrap();
    let mut spec = crate::pane::PaneSpec {
        command: vec!["claude".into(), "--resume".into(), "conversation".into()],
        account: Some("claude:default".into()),
        session: Some("conversation".into()),
        ..Default::default()
    };
    all.add(acct("claude", "patron.bzh", "/patron")).unwrap();
    all.add(acct("claude", "gmail", "/gmail")).unwrap();
    all.rules.push(Rule {
        workspace: Some("old".into()),
        path: None,
        agent: Some("claude".into()),
        accounts: vec!["default".into()],
        account_ids: None,
    });
    all.adopt_checked(&original, None).unwrap();
    let pending = all.reviews().pop().unwrap();
    let target = all.get("claude", "patron.bzh").unwrap();
    assert_eq!(label(&all.get("claude", "default").unwrap()), "claude·Existing account");
    assert_eq!(all.all().len(), 2);
    all.bind_pane(&mut spec);
    let command = spec.command.clone();
    let home_env = spec.env.clone();
    all.review(&pending, Some(&target)).unwrap();
    all.bind_pane(&mut spec);
    assert_eq!(spec.command, command);
    assert_eq!(spec.session.as_deref(), Some("conversation"));
    assert_eq!(spec.account_id.as_deref(), Some(target.id.as_str()));
    assert_eq!(spec.account.as_deref(), Some("claude:patron.bzh"));
    assert_eq!(
        all.pane_account(&crate::pane::PaneSpec {
            account: Some("claude:default".into()),
            env: home_env.to_vec(),
            ..Default::default()
        })
        .unwrap()
        .home,
        "~/.claude"
    );
    assert_eq!(all.at_launch_home(target.clone(), &spec.env).home, "~/.claude");
    assert_eq!(all.by_home("claude", &paths::expand_tilde("~/.claude")).unwrap().id, target.id);
    assert_eq!(all.by_id(&original.id).unwrap().id, target.id);
    assert_eq!(all.get("claude", "claude:default").unwrap().id, target.id);
    assert_eq!(all.get("claude", "default").unwrap().home, target.home, "new launches use the chosen home");
    assert_eq!(all.choose("claude", Some("old"), None, &UsageMap::default(), 0).unwrap().id, target.id);
    assert!(all.reviews().is_empty());
    assert_eq!(all.all().len(), 2);
    assert!(all.review(&pending, Some(&target)).is_err(), "a second confirmation is stale");
}

#[test]
fn a_virtual_default_identity_never_moves_to_a_replacement_home() {
    let mut all = Accounts::empty();
    let old = acct("claude", "Personal", "~/.claude");
    all.accounts.push(old.clone());
    assert_eq!(all.by_id("default-claude").unwrap().id, old.id);
    assert!(all.remove("claude", "Personal"));
    let replacement = all.add(Account { id: new_uuid(), ..old.clone() }).unwrap();
    assert_ne!(replacement.id, "default-claude");
    assert!(all.by_id("default-claude").is_none());
    assert!(all.get("claude", "@default-claude").is_none());
    assert!(all.by_id(&old.id).is_none());
    assert_eq!(all.get("claude", "default").unwrap().id, replacement.id, "names may select the replacement");
}

#[test]
fn stale_review_choices_leave_the_pending_account_intact() {
    let mut all = Accounts::empty();
    let target = all.add(acct("claude", "work", "/work")).unwrap();
    let original = all.get("claude", "default").unwrap();
    all.adopt_checked(&original, None).unwrap();
    let pending = all.reviews().pop().unwrap();
    all.remove("claude", "work");
    all.add(Account { id: new_uuid(), ..target.clone() }).unwrap();
    let before = all.text().unwrap();
    assert!(all.review(&pending, Some(&target)).is_err());
    assert_eq!(all.text().unwrap(), before);
    let mut moved = target;
    moved.home = "/moved".into();
    assert!(all.review(&pending, Some(&moved)).is_err());
    assert_eq!(all.text().unwrap(), before);
}

#[test]
fn persisted_review_preserves_the_original_file_and_is_not_offered_again() {
    let dir = std::env::temp_dir().join(format!("kraal-review-file-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("accounts.toml");
    let source = "# My accounts.\n[[account]]\nid = 'old-default'\nagent = 'claude'\nname = 'default'\nhome = '~/.claude'\nnote = 'keep me'\nfuture = 7\n\n[[account]]\nid = 'patron'\nagent = 'claude'\nname = 'patron.bzh'\nhome = '/patron'\n";
    std::fs::write(&path, source).unwrap();
    let existing = Accounts::load_from(&path).unwrap().unlisted().into_iter().find(|a| a.agent == "claude").unwrap();
    Accounts::update_at(&path, |all| all.adopt_checked(&existing, None)).unwrap();
    let all = Accounts::load_from(&path).unwrap();
    let pending = all.reviews().pop().unwrap();
    let chosen = all.get("claude", "patron.bzh").unwrap();
    Accounts::update_at(&path, |all| all.review(&pending, Some(&chosen))).unwrap();
    let all = Accounts::load_from(&path).unwrap();
    assert!(all.reviews().is_empty());
    assert_eq!(all.get("claude", "default").unwrap().id, chosen.id);
    assert_eq!(all.by_id(&existing.id).unwrap().id, chosen.id);
    assert!(all.unlisted().iter().all(|a| a.agent != "claude"));
    assert_eq!(all.all().len(), 1);
    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(saved.contains("# My accounts.") && saved.contains("note = 'keep me'") && saved.contains("future = 7"));
    Accounts::update_at(&path, |all| all.adopt_checked(&existing, None)).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), saved, "late discovery cannot undo a review");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn invalid_links_and_failed_review_writes_leave_the_file_unchanged() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("kraal-review-failure-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("accounts.toml");
    for (target, agent) in [("a", "claude"), ("b", "codex"), ("../bad", "claude")] {
        let source = format!(
            "[[account]]\nid = 'a'\nname = 'one'\nagent = 'claude'\nhome = '/one'\nlinked_to = '{target}'\n[[account]]\nid = 'b'\nname = 'two'\nagent = '{agent}'\nhome = '/two'\n"
        );
        std::fs::write(&path, &source).unwrap();
        assert!(Accounts::update_at(&path, |_| Ok(())).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
    }
    let mut all = Accounts::empty();
    all.path = path.clone();
    all.add(acct("claude", "work", "/work")).unwrap();
    let existing = all.get("claude", "default").unwrap();
    all.adopt_checked(&existing, None).unwrap();
    all.save().unwrap();
    let pending = all.reviews().pop().unwrap();
    let before = std::fs::read_to_string(&path).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    let result = Accounts::update_at(&path, |all| all.review(&pending, None));
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    if result.is_err() {
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        assert_eq!(Accounts::load_from(&path).unwrap().reviews().len(), 1);
        Accounts::update_at(&path, |all| all.review(&pending, None)).unwrap();
    }
    assert!(Accounts::load_from(&path).unwrap().reviews().is_empty());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn same_canonical_home_is_one_account_and_keeps_legacy_names_ids_and_notes() {
    let dir = std::env::temp_dir().join(format!("kraal-same-home-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("home")).unwrap();
    let alias = dir.join("alias");
    std::os::unix::fs::symlink(dir.join("home"), &alias).unwrap();
    let path = dir.join("accounts.toml");
    let source = format!(
        "# Mine.\n[[account]]\nid = 'work'\nname = 'work'\nagent = 'claude'\nhome = {:?}\n\n# Keep this note.\n[[account]]\nid = 'other'\nname = 'other'\nagent = 'claude'\nhome = {:?}\nnote = 'old label'\nfuture = 7\n",
        dir.join("home").display().to_string(),
        alias.display().to_string()
    );
    std::fs::write(&path, &source).unwrap();
    let all = Accounts::load_from(&path).unwrap();
    let first = all.get("claude", "work").unwrap();
    assert_eq!(all.all().len(), 1);
    assert_eq!(all.get("claude", "other").unwrap().id, first.id);
    let old_alias = all.accounts.iter().find(|a| a.name == "other").unwrap().id.clone();
    assert_eq!(all.by_id(&old_alias).unwrap().id, first.id);
    Accounts::update_at(&path, |_| Ok(())).unwrap();
    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(
        saved.contains("# Keep this note.") && saved.contains("future = 7") && saved.contains("note = 'old label'")
    );
    assert!(saved.contains("[[account.references]]"));
    assert_eq!(saved.matches("[[account]]").count(), 1);
    let mut all = Accounts::load_from(&path).unwrap();
    let added = all.add(acct("claude", "third", alias.to_str().unwrap())).unwrap();
    assert_eq!(added.id, first.id);
    assert_eq!(all.all().len(), 1);
    assert_eq!(all.get("claude", "third").unwrap().id, first.id);
    all.add(acct("codex", "work", alias.to_str().unwrap())).unwrap();
    assert_eq!(all.all().len(), 2, "different agents remain separate");
    std::fs::remove_dir_all(dir).unwrap();
}

fn checked_identity(email: &str, organization: Option<&str>, plan: Option<&str>) -> crate::account_status::Checked {
    crate::account_status::Checked {
        status: crate::account_status::Status::SignedIn,
        identity: crate::account_status::Identity::from_status(&serde_json::json!({
            "email": email, "orgId": organization, "subscriptionType": plan,
        })),
    }
}

#[test]
fn identity_join_requires_one_exact_public_identity_and_saves_the_answer_once() {
    use crate::account_status::{Checked, Status, reconcile};
    for case in [
        "unique",
        "email",
        "two",
        "unreadable",
        "unreadable-other",
        "failed",
        "organization",
        "plan",
        "missing-context",
        "other-email",
        "other-agent",
        "stale",
        "login-pending",
        "login-finished",
        "added",
        "personal",
        "pending",
    ] {
        let mut all = Accounts::empty();
        let existing = all.get("claude", "default").unwrap();
        let context = (case != "email").then_some("org-1");
        let checked = if case == "unreadable" || case == "personal" {
            Checked { status: Status::SignedIn, identity: None }
        } else if case == "failed" {
            Checked { status: Status::CouldNotCheck, identity: None }
        } else {
            checked_identity("me@example.com", context, if case == "email" { None } else { Some("max") })
        };
        let mut named = Vec::new();
        if case != "personal" {
            let target =
                all.add(acct(if case == "other-agent" { "codex" } else { "claude" }, "patron.bzh", "/patron")).unwrap();
            let identity = checked_identity(
                if case == "other-email" { "other@example.com" } else { "me@example.com" },
                if case == "organization" {
                    Some("org-2")
                } else if case == "missing-context" {
                    None
                } else {
                    context
                },
                if case == "email" { None } else { Some(if case == "plan" { "pro" } else { "max" }) },
            );
            named.push((target, identity));
        }
        if matches!(case, "two" | "added" | "unreadable-other") {
            let second = all.add(acct("claude", "gmail", "/gmail")).unwrap();
            if case == "two" {
                named.push((second, checked_identity("me@example.com", context, Some("max"))));
            } else if case == "unreadable-other" {
                named.push((second, Checked { status: Status::CouldNotCheck, identity: None }));
            }
        }
        if case == "stale" {
            named[0].0.home = "/old-home".into();
        }
        if matches!(case, "login-pending" | "login-finished") {
            all.set_sign_in_required(&named[0].0, true);
            if case == "login-pending" {
                // The status command still reports A while an unfinished login is changing it.
                named[0].0 = all.by_id(&named[0].0.id).unwrap();
            } else {
                all.set_sign_in_required(&named[0].0, false);
            }
        }
        if case == "pending" {
            all.adopt_checked(&existing, None).unwrap();
        }
        let notice = reconcile(&mut all, &existing, &checked, &named).unwrap();
        if matches!(case, "unique" | "email" | "pending") {
            assert_eq!(notice.as_deref(), Some("~/.claude is Claude · patron.bzh"), "{case}");
            assert!(all.reviews().is_empty());
            assert_eq!(all.get("claude", "default").unwrap().name, "patron.bzh");
            assert_eq!(all.get("claude", "default").unwrap().home, "/patron");
            assert_eq!(reconcile(&mut all, &existing, &checked, &named).unwrap(), None, "the notice is emitted once");
        } else if case == "personal" || case == "other-agent" {
            assert!(notice.is_none());
            assert!(all.reviews().is_empty());
            assert_eq!(all.for_agent("claude").len(), 1);
            assert_eq!(all.for_agent("claude")[0].home, existing.home);
            if case == "personal" {
                assert_eq!(all.for_agent("claude")[0].name, "Personal");
            }
        } else {
            assert!(notice.is_none(), "{case}");
            assert_eq!(all.reviews().len(), 1, "{case}");
        }
    }
}

#[test]
fn automatic_join_keeps_pane_launch_home_and_persists_without_repeating_the_notice() {
    let dir = std::env::temp_dir().join(format!("kraal-auto-join-{}", new_uuid()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("accounts.toml");
    let (existing, target) = Accounts::update_at(&file, |all| {
        let existing = all.get("claude", "default").unwrap();
        let target = all.add(acct("claude", "patron.bzh", "/patron"))?;
        Ok((existing, target))
    })
    .unwrap();
    let mut pane = crate::pane::PaneSpec {
        account: Some("claude:default".into()),
        command: vec!["claude".into()],
        session: Some("conversation".into()),
        ..Default::default()
    };
    Accounts::load_from(&file).unwrap().bind_pane(&mut pane);
    let before = pane.clone();
    let checked = checked_identity("me@example.com", Some("org"), Some("max"));
    let named = vec![(target.clone(), checked.clone())];
    let notice =
        Accounts::update_at(&file, |all| crate::account_status::reconcile(all, &existing, &checked, &named)).unwrap();
    assert_eq!(notice.as_deref(), Some("~/.claude is Claude · patron.bzh"));
    let all = Accounts::load_from(&file).unwrap();
    all.bind_pane(&mut pane);
    assert_eq!(pane.command, before.command);
    assert_eq!(pane.session, before.session);
    assert_eq!(
        pane.env.iter().find(|(k, _)| k == "CLAUDE_CONFIG_DIR"),
        before.env.iter().find(|(k, _)| k == "CLAUDE_CONFIG_DIR")
    );
    assert_eq!(pane.account_id.as_deref(), Some(target.id.as_str()));
    assert_eq!(all.at_launch_home(target.clone(), &pane.env).home, existing.home);
    assert_eq!(all.pane_account(&pane).unwrap().home, existing.home);
    assert_eq!(all.get("claude", "default").unwrap().home, target.home);
    let saved = std::fs::read_to_string(&file).unwrap();
    assert_eq!(toml::from_str::<toml::Table>(&saved).unwrap()["account"].as_array().unwrap().len(), 1);
    assert!(saved.contains("[[account.references]]"));
    assert_eq!(
        Accounts::update_at(&file, |all| crate::account_status::reconcile(all, &existing, &checked, &named)).unwrap(),
        None
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), saved);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn linked_usage_uses_the_latest_observation_and_never_adds_two_homes() {
    let dir = std::env::temp_dir().join(format!("kraal-linked-usage-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("by-id")).unwrap();
    let mut all = Accounts::empty();
    let target = all.add(acct("claude", "patron.bzh", "/patron")).unwrap();
    let original = all.get("claude", "default").unwrap();
    let checked = checked_identity("me@example.com", Some("org"), Some("max"));
    assert!(
        crate::account_status::reconcile(&mut all, &original, &checked, &[(target.clone(), checked.clone())])
            .unwrap()
            .is_some()
    );
    let observation = |percent, at| Usage {
        observed_at: at,
        five_hour: Some(Window { used_percent: percent, resets_at: None }),
        ..Default::default()
    };
    for (account, usage) in [(&original, observation(57.0, 200)), (&target, observation(40.0, 100))] {
        std::fs::write(
            dir.join("by-id").join(usage_file(&account.agent, &account.id)),
            serde_json::to_vec(&usage).unwrap(),
        )
        .unwrap();
    }
    let usage = UsageMap::load_from(&all, &dir);
    assert_eq!(usage.0.len(), 1);
    assert_eq!(usage.peak(&target, 200), Some(57.0));
    std::fs::write(
        dir.join("by-id").join(usage_file(&target.agent, &target.id)),
        serde_json::to_vec(&observation(20.0, 300)).unwrap(),
    )
    .unwrap();
    assert_eq!(UsageMap::load_from(&all, &dir).peak(&target, 300), Some(20.0));
    assert!(dir.join("by-id").join(usage_file(&original.agent, &original.id)).exists());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn agents_started_directly_resume_with_their_arguments() {
    let claude = agent_kind("claude").unwrap();
    let argv = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
    assert_eq!(
        resume_argv(claude, &argv("/usr/bin/claude --model opus -c"), "abc").unwrap(),
        (argv("/usr/bin/claude --resume abc --model opus"), true)
    );
    assert_eq!(resume_argv(claude, &argv("claude --resume old"), "abc").unwrap(), (argv("claude --resume abc"), true));
    assert_eq!(
        resume_argv(claude, &argv("claude -r --model opus"), "abc").unwrap(),
        (argv("claude --resume abc --model opus"), true),
        "a picker's -r has no value: the option after it stays"
    );
    assert_eq!(
        resume_argv(claude, &argv("claude --model opus --remote-control"), "abc").unwrap(),
        (argv("claude --resume abc --model opus --remote-control"), true),
        "kraal agent --remote stays on Remote Control"
    );
    assert_eq!(
        resume_argv(claude, &argv("claude --remote-control Phone --model opus"), "abc").unwrap(),
        (argv("claude --resume abc --remote-control Phone --model opus"), true),
        "the word after it names the session"
    );
    assert_eq!(resumable_args(claude, &argv("--rc Phone")).unwrap(), (argv("--rc Phone"), true));
    assert_eq!(resumable_args(claude, &argv("--rc=Phone fix")).unwrap(), (argv("--rc=Phone"), false), "a prompt");
    assert!(resume_argv(claude, &argv("claude"), "x; rm -rf /").is_none());
    assert!(resume_argv(claude, &argv("vim"), "abc").is_none());
    let codex = agent_kind("codex").unwrap();
    assert_eq!(resume_argv(codex, &argv("codex -m o3"), "abc").unwrap(), (argv("codex resume abc -m o3"), true));
    assert!(resume_argv(codex, &argv("codex exec hi"), "abc").is_none());
    assert!(resume_argv(codex, &argv("codex -m o3 exec hi"), "abc").is_none());
    // A one-shot run is never resumed; a prompt is never sent again, and the pane then
    // waits for Enter.
    assert!(resume_argv(claude, &argv("claude -p deploy"), "abc").is_none());
    assert!(resume_argv(claude, &argv("claude --model opus --print deploy"), "abc").is_none());
    assert_eq!(
        resume_argv(claude, &argv("claude --model opus deploy"), "abc").unwrap(),
        (argv("claude --resume abc --model opus"), false)
    );
    assert_eq!(
        resume_argv(claude, &argv("claude --add-dir a b --verbose -- deploy"), "abc").unwrap(),
        (argv("claude --resume abc --add-dir a b --verbose"), false)
    );
    assert_eq!(resume_argv(codex, &argv("codex -m o3 fix"), "abc").unwrap(), (argv("codex resume abc -m o3"), false));
    // Option values are kept: dropping one would break the command.
    assert_eq!(
        resume_argv(claude, &argv("claude --effort high"), "abc").unwrap(),
        (argv("claude --resume abc --effort high"), true)
    );
    assert_eq!(
        resume_argv(codex, &argv("codex --enable web_search"), "abc").unwrap(),
        (argv("codex resume abc --enable web_search"), true)
    );
    assert_eq!(
        resume_argv(claude, &argv("claude --verbose deploy"), "abc").unwrap(),
        (argv("claude --resume abc --verbose"), false),
        "a word after a flag that takes no value is a prompt"
    );
    // After an option kraal does not know, or one whose value is optional, everything stays
    // as given (a value is never dropped), and the pane waits for Enter.
    assert_eq!(
        resume_argv(claude, &argv("claude --new-option x"), "abc").unwrap(),
        (argv("claude --resume abc --new-option x"), false)
    );
    assert_eq!(
        resume_argv(claude, &argv("claude --new-option a b --verbose c"), "abc").unwrap(),
        (argv("claude --resume abc --new-option a b --verbose c"), false)
    );
    assert_eq!(
        resume_argv(claude, &argv("claude --debug api deploy"), "abc").unwrap(),
        (argv("claude --resume abc --debug api deploy"), false)
    );
    assert!(resume_argv(claude, &argv("claude --debug --verbose"), "abc").unwrap().1);
    // Options that take several values keep them all.
    assert_eq!(
        resume_argv(claude, &argv("claude --file file_abc:doc.txt file_def:img.png"), "abc").unwrap(),
        (argv("claude --resume abc --file file_abc:doc.txt file_def:img.png"), true)
    );
    // A flag the agent knows takes no value: the word after it is a prompt, never resent.
    assert_eq!(
        resume_argv(claude, &argv("claude --chrome Deploy"), "abc").unwrap(),
        (argv("claude --resume abc --chrome"), false)
    );
    assert!(resume_argv(claude, &argv("claude --help"), "abc").is_none());
    // A one-shot run says so anywhere among the options, even after one kraal cannot read.
    assert!(resume_argv(claude, &argv("claude --debug api --print Deploy"), "abc").is_none());
    assert!(resume_argv(claude, &argv("claude --brand-new x -p Deploy"), "abc").is_none());
    // A subcommand runs on its own only where a subcommand goes: the first word that is not
    // an option or an option's value.
    assert!(resume_argv(claude, &argv("claude update"), "abc").is_none());
    assert!(resume_argv(claude, &argv("claude mcp list"), "abc").is_none());
    assert!(resume_argv(claude, &argv("claude --model opus update"), "abc").is_none());
    assert!(resume_argv(claude, &argv("claude --verbose mcp list"), "abc").is_none());
    // After an option whose value is optional or unknown, which word is the subcommand cannot
    // be told: nothing is taken for a one-shot run, everything stays, and the command is shown
    // before it runs. `--worktree update` names a worktree, `update` is not run.
    assert_eq!(
        resume_argv(claude, &argv("claude --worktree update Deploy"), "abc").unwrap(),
        (argv("claude --resume abc --worktree update Deploy"), false)
    );
    assert_eq!(
        resume_argv(claude, &argv("claude -w update Deploy"), "abc").unwrap(),
        (argv("claude --resume abc -w update Deploy"), false)
    );
    assert_eq!(
        resume_argv(codex, &argv("codex --brand-new x exec hi"), "abc").unwrap(),
        (argv("codex resume abc --brand-new x exec hi"), false)
    );
    // A value given with `=` to an option that takes several keeps the ones after it.
    assert_eq!(
        resume_argv(claude, &argv("claude --file=file_abc:doc.txt file_def:img.png"), "abc").unwrap(),
        (argv("claude --resume abc --file=file_abc:doc.txt file_def:img.png"), true)
    );
    assert_eq!(
        resume_argv(claude, &argv("claude --model=opus Deploy"), "abc").unwrap(),
        (argv("claude --resume abc --model=opus"), false)
    );
    // After `--opt=value` of an option kraal does not know, a word may be another value.
    assert_eq!(
        resume_argv(claude, &argv("claude --brand-new=a b"), "abc").unwrap(),
        (argv("claude --resume abc --brand-new=a b"), false)
    );
    // A Codex fork or resume is a conversation: the one it made is resumed, without the id
    // it was taken from or its prompt (never sent again).
    assert_eq!(resume_argv(codex, &argv("codex fork parent"), "child").unwrap(), (argv("codex resume child"), true));
    assert_eq!(
        resume_argv(codex, &argv("codex fork parent Deploy"), "child").unwrap(),
        (argv("codex resume child"), false)
    );
    assert_eq!(
        resume_argv(codex, &argv("codex -m o3 fork --last --search Deploy"), "child").unwrap(),
        (argv("codex resume child -m o3 --search"), false)
    );
    assert_eq!(
        resume_argv(codex, &argv("codex fork --all -i a.png b.png parent"), "child").unwrap(),
        (argv("codex resume child -i a.png b.png parent"), true),
        "-i takes every word after it, as Codex's own parser does"
    );
    assert_eq!(
        resume_argv(codex, &argv("codex resume old -m o3"), "abc").unwrap(),
        (argv("codex resume abc -m o3"), true)
    );
    assert_eq!(resume_argv(codex, &argv("codex resume --last"), "abc").unwrap(), (argv("codex resume abc"), true));
    assert_eq!(
        resumable_args(codex, &argv("fork parent Deploy")).unwrap(),
        (Vec::new(), false),
        "started from an account, the same"
    );
}

#[test]
fn conversations_are_found_by_their_transcript() {
    let home = std::env::temp_dir().join(format!("kraal-conv-{}", std::process::id()));
    let a = Account {
        id: crate::identity::new_uuid(),
        name: "t".into(),
        agent: "claude".into(),
        home: home.display().to_string(),
        threshold: None,
        note: None,
        sign_in_required: false,
        login_revision: String::new(),
        review: false,
        public_identity: None,
        linked_to: None,
        references: Vec::new(),
    };
    assert_eq!(conversation_exists(&a, "abc").unwrap(), Some(false));
    std::fs::create_dir_all(home.join("projects/-home-me")).unwrap();
    std::fs::write(home.join("projects/-home-me/abc.jsonl"), "").unwrap();
    assert_eq!(conversation_exists(&a, "abc").unwrap(), Some(true));
    assert_eq!(conversation_exists(&a, "other").unwrap(), Some(false));
    // Files that cannot be read do not say "no conversation".
    use std::os::unix::fs::PermissionsExt;
    let projects = home.join("projects");
    std::fs::set_permissions(&projects, std::fs::Permissions::from_mode(0o000)).unwrap();
    let unreadable = std::fs::read_dir(&projects).is_err(); // not when tests run as root
    if unreadable {
        assert!(conversation_exists(&a, "abc").is_err());
    }
    std::fs::set_permissions(&projects, std::fs::Permissions::from_mode(0o755)).unwrap();
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn login_status_commands() {
    let a = Accounts::empty();
    let argv = |agent: &str| login_status(&a.get(agent, "default").unwrap()).unwrap().argv;
    assert_eq!(argv("claude"), vec!["claude", "auth", "status"]);
    assert_eq!(argv("codex"), vec!["codex", "login", "status"]);
}

#[test]
fn launch_args_are_what_the_user_gave() {
    let claude = agent_kind("claude").unwrap();
    let argv = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
    assert_eq!(launch_args(claude, &argv("claude --session-id abc --model opus")), argv("--model opus"));
    assert_eq!(launch_args(claude, &argv("claude --resume abc --model opus")), argv("--model opus"));
    assert!(launch_args(claude, &argv("claude")).is_empty());
    let codex = agent_kind("codex").unwrap();
    assert_eq!(launch_args(codex, &argv("codex resume abc -m o3")), argv("-m o3"));
    assert_eq!(launch_args(codex, &argv("codex -m o3")), argv("-m o3"));
}

pub(super) fn acct(agent: &str, name: &str, home: &str) -> Account {
    let mut a = Account {
        id: String::new(),
        name: name.into(),
        agent: agent.into(),
        home: home.into(),
        threshold: None,
        note: None,
        sign_in_required: false,
        login_revision: String::new(),
        review: false,
        public_identity: None,
        linked_to: None,
        references: Vec::new(),
    };
    a.id = format!("legacy-{:016x}", crate::text::fnv64(&format!("{agent}\0{name}\0{home}")));
    a
}

#[test]
fn unfinished_sign_in_survives_reload_and_verification_clears_only_its_home() {
    let dir = std::env::temp_dir().join(format!("kraal-sign-in-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("accounts.toml");
    let a = acct("claude", "work", dir.to_str().unwrap());
    let alias = Account { id: new_uuid(), name: "alias".into(), ..a.clone() };
    let other = acct("codex", "work", dir.to_str().unwrap());
    let mut all = Accounts::load_from(&path).unwrap();
    all.add(a.clone()).unwrap();
    all.add(alias).unwrap();
    all.add(other).unwrap();
    all.save().unwrap();
    let original = std::fs::read_to_string(&path).unwrap();
    assert!(!original.contains("sign_in_required"));
    Accounts::update_at(&path, |all| {
        all.set_sign_in_required(&a, true);
        Ok(())
    })
    .unwrap();
    let mut loaded = Accounts::load_from(&path).unwrap();
    assert!(loaded.get("claude", "work").unwrap().sign_in_required);
    assert!(loaded.get("claude", "alias").unwrap().sign_in_required);
    assert!(!loaded.get("codex", "work").unwrap().sign_in_required);
    let revision = loaded.get("claude", "work").unwrap().login_revision;
    assert!(!revision.is_empty());
    assert_eq!(loaded.get("claude", "alias").unwrap().login_revision, revision);
    assert!(loaded.get("codex", "work").unwrap().login_revision.is_empty());
    loaded.set_sign_in_required(&a, false);
    loaded.save().unwrap();
    let saved = std::fs::read_to_string(&path).unwrap();
    let revision_line = format!("login_revision = {revision:?}\n");
    assert_eq!(saved.matches(&revision_line).count(), 1);
    assert_eq!(saved.replace(&revision_line, ""), original);
    assert_eq!(Accounts::load_from(&path).unwrap().get("claude", "work").unwrap().login_revision, revision);
    loaded.remove("claude", "work");
    loaded.remove("claude", "alias");
    loaded.set_sign_in_required(&a, false);
    assert!(loaded.get("claude", "work").is_none(), "a late check must not recreate a removed account");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn the_same_sign_in_is_recognized_through_a_directory_alias() {
    let dir = std::env::temp_dir().join(format!("kraal-sign-in-alias-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("home")).unwrap();
    std::os::unix::fs::symlink(dir.join("home"), dir.join("alias")).unwrap();
    let a = acct("claude", "work", dir.join("home").to_str().unwrap());
    let b = acct("claude", "renamed", dir.join("alias").to_str().unwrap());
    assert!(a.same_home(&b));
    assert!(!a.same_home(&Account { agent: "codex".into(), ..b }));
    let alias = acct("claude", "alias", dir.join("alias").to_str().unwrap());
    let held = lock_login(&a).unwrap();
    assert!(lock_login(&alias).unwrap_err().to_string().contains("sign-in already running"));
    let waiter = held.clone();
    drop(held);
    assert!(lock_login(&alias).is_err(), "a closed pane's waiter still holds the login");
    drop(waiter);
    // Another test's fork can hold this descriptor until exec closes it (as with the
    // session locks). Releasing our last hold must still make the lock available.
    let start = std::time::Instant::now();
    loop {
        if let Ok(lock) = lock_login(&alias) {
            drop(lock);
            break;
        }
        assert!(start.elapsed() < crate::testing::PATIENCE, "the sign-in lock was never released");
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    std::fs::remove_dir_all(dir).unwrap();
}

fn accounts() -> Accounts {
    let mut a = Accounts::empty();
    a.accounts = vec![acct("claude", "pro", "/tmp/kraal-test-claude-pro"), acct("claude", "perso", "~/.claude")];
    a.rules = vec![Rule {
        account_ids: None,
        workspace: Some("work".into()),
        path: None,
        agent: None,
        accounts: vec!["pro".into()],
    }];
    a
}

#[test]
fn legacy_default_is_resolvable_but_never_listed() {
    let a = Accounts::empty();
    let codex = a.for_agent("codex");
    assert!(codex.is_empty());
    assert_eq!(a.get("codex", "default").unwrap().home, "~/.codex");
    // When an account uses the default home, no implicit one is added.
    let a = accounts();
    let names: Vec<String> = a.for_agent("claude").into_iter().map(|x| x.name).collect();
    assert_eq!(names, vec!["pro", "perso"]);
    // "default" names the default home only.
    let mut a = Accounts::empty();
    assert!(a.add(acct("codex", "default", "/tmp/kraal-test-other")).is_err());
    a.accounts.push(acct("codex", "default", "/tmp/kraal-test-other"));
    assert_eq!(a.get("codex", "default").unwrap().home, "/tmp/kraal-test-other", "an older one stays reachable");
}

#[test]
fn account_homes_are_kept_absolute() {
    assert_eq!(absolute_home("~/.claude-x").unwrap(), "~/.claude-x");
    assert_eq!(absolute_home("/srv/x").unwrap(), "/srv/x");
    let rel = absolute_home("agent-home").unwrap();
    assert!(Path::new(&rel).is_absolute() && rel.ends_with("agent-home"), "{rel}");
}

#[test]
fn rules_and_thresholds_choose() {
    let a = accounts();
    let mut usage = UsageMap::default();
    let now = 1_000_000;
    assert_eq!(a.choose("claude", Some("work"), None, &usage, now).unwrap().name, "pro");
    assert_eq!(a.choose("claude", Some("other"), None, &usage, now).unwrap().name, "pro");
    usage.0.insert(
        a.get("claude", "pro").unwrap().key(),
        Usage { five_hour: Some(Window { used_percent: 95.0, resets_at: Some(now + 600) }), ..Default::default() },
    );
    // Over the threshold: the rule has no other account, so it takes the one that resets first.
    assert_eq!(a.choose("claude", Some("work"), None, &usage, now).unwrap().name, "pro");
    assert_eq!(a.choose("claude", None, None, &usage, now).unwrap().name, "perso");
    let pro = a.get("claude", "pro").unwrap();
    // The "work" rule only lists pro: no alternative there, but elsewhere perso is fine.
    assert!(a.alternative(&pro, Some("work"), None, &usage, now, |_| false).is_none());
    assert!(a.alternative(&pro, None, None, &usage, now, |_| false).is_none(), "unknown usage is not zero");
    usage.0.insert(
        a.get("claude", "perso").unwrap().key(),
        Usage { five_hour: Some(Window { used_percent: 0.0, resets_at: Some(now + 600) }), ..Default::default() },
    );
    assert_eq!(a.alternative(&pro, None, None, &usage, now, |_| false).unwrap().name, "perso");
    // A window that already reset does not count.
    assert_eq!(usage.peak(&pro, now + 601), Some(0.0));
}

#[test]
fn unknown_usage_and_sign_in_required_accounts_are_not_threshold_alternatives() {
    let mut a = accounts();
    let pro = a.get("claude", "pro").unwrap();
    let perso = a.get("claude", "perso").unwrap();
    let mut usage = UsageMap::default();
    assert_eq!(usage.peak(&perso, 0), None);
    usage.0.insert(perso.key(), Usage::default());
    assert_eq!(usage.peak(&perso, 0), None);
    assert!(a.alternative(&pro, None, None, &usage, 0, |_| false).is_none());
    usage.0.insert(
        a.get("claude", "perso").unwrap().key(),
        Usage { five_hour: Some(Window { used_percent: 0.0, resets_at: Some(100) }), ..Default::default() },
    );
    assert_eq!(a.choose("claude", None, None, &usage, 0).unwrap().name, "perso", "known headroom before unknown");
    assert_eq!(a.alternative(&pro, None, None, &usage, 0, |_| false).unwrap().name, "perso");
    a.set_sign_in_required(&perso, true);
    assert!(a.alternative(&pro, None, None, &usage, 0, |_| false).is_none());
    a.set_sign_in_required(&perso, false);
    a.accounts.iter_mut().find(|a| a.name == "perso").unwrap().home = pro.home.clone();
    assert!(a.alternative(&pro, None, None, &usage, 0, |_| false).is_none(), "an alias is the same account");
}

#[test]
fn a_new_agent_does_not_start_on_an_account_awaiting_sign_in() {
    let mut a = accounts();
    let (pro, perso) = (a.get("claude", "pro").unwrap(), a.get("claude", "perso").unwrap());
    let mut usage = UsageMap::default();
    usage.0.insert(
        pro.key(),
        Usage { five_hour: Some(Window { used_percent: 95.0, resets_at: Some(600) }), ..Default::default() },
    );
    assert_eq!(a.choose("claude", None, None, &usage, 0).unwrap().name, "perso", "unknown usage before a full one");
    a.set_sign_in_required(&perso, true);
    assert_eq!(a.choose("claude", None, None, &usage, 0).unwrap().name, "pro");
    a.set_sign_in_required(&pro, true);
    assert_eq!(a.choose("claude", None, None, &usage, 0).unwrap().name, "perso", "unless all await it");
}

#[test]
fn a_worktree_anywhere_waits_for_git_before_any_account() {
    let dir = std::env::temp_dir().join(format!("kraal-rule-worktree-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let repo = dir.join("company/repo");
    std::fs::create_dir_all(&repo).unwrap();
    let git = |cwd: &Path, args: &[&str]| {
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        assert!(ok.status.success(), "{args:?}: {}", String::from_utf8_lossy(&ok.stderr));
    };
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "one"]);
    // A linked worktree far from the repository, not in kraal's `<repo>.worktrees/`.
    let feature = dir.join("elsewhere/feature");
    git(&repo, &["worktree", "add", "-q", "-b", "feature", feature.to_str().unwrap()]);
    let repo = std::fs::canonicalize(&repo).unwrap();
    let feature = std::fs::canonicalize(&feature).unwrap();
    let mut a = accounts();
    a.rules = vec![Rule {
        account_ids: None,
        workspace: None,
        path: Some(repo.display().to_string()),
        agent: None,
        accounts: vec!["pro".into()],
    }];
    let usage = UsageMap::default();
    // Until git has said where the worktree belongs, no choice is made from the
    // unrestricted list: the caller asks git first.
    assert!(a.undecided("claude", Some(&feature)));
    assert!(!a.undecided("claude", Some(&repo.join("src"))), "inside the rule's own path");
    crate::git::project_path(&feature);
    assert!(!a.undecided("claude", Some(&feature)));
    assert_eq!(a.choose("claude", None, Some(&feature), &usage, 0).unwrap().name, "pro");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_conversation_follows_to_an_account_that_keeps_its_own() {
    let dir = std::env::temp_dir().join(format!("kraal-carry-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (from, to) = (dir.join("perso"), dir.join("pro"));
    std::fs::create_dir_all(from.join("projects/-work-app")).unwrap();
    std::fs::create_dir_all(to.join("projects")).unwrap();
    std::fs::write(from.join("projects/-work-app/abc-1.jsonl"), "{}\n").unwrap();
    let a = acct("claude", "perso", from.to_str().unwrap());
    let b = acct("claude", "pro", to.to_str().unwrap());
    assert_eq!(conversation_exists(&b, "abc-1").unwrap(), Some(false));
    assert!(carry_conversation(&a, &b, "abc-1").unwrap());
    assert_eq!(conversation_exists(&b, "abc-1").unwrap(), Some(true));
    assert!(carry_conversation(&a, &b, "abc-1").unwrap(), "again: already there");
    assert!(carry_conversation(&a, &b, "nope").is_err(), "a missing transcript is not a new conversation");
    let codex = acct("codex", "pro", to.to_str().unwrap());
    assert!(!carry_conversation(&codex, &codex, "abc-1").unwrap());
    // The conversation went on on pro; switching back to perso takes all of it, never the
    // older copy perso still has.
    let (on_from, on_to) = (from.join("projects/-work-app/abc-1.jsonl"), to.join("projects/-work-app/abc-1.jsonl"));
    std::fs::write(&on_to, "{}\n{\"more\":1}\n").unwrap();
    assert!(carry_conversation(&b, &a, "abc-1").unwrap());
    assert_eq!(std::fs::read_to_string(&on_from).unwrap(), "{}\n{\"more\":1}\n");
    assert!(carry_conversation(&b, &a, "abc-1").unwrap(), "perso has all of it now");
    assert!(carry_conversation(&a, &b, "abc-1").unwrap(), "pro has all of it too");
    // Each went on on its own: neither copy holds the other, continuing would lose a part.
    std::fs::write(&on_from, "{}\n{\"more\":1}\n{\"perso\":1}\n").unwrap();
    std::fs::write(&on_to, "{}\n{\"more\":1}\n{\"pro\":1}\n").unwrap();
    assert!(check_carry(&a, &b, "abc-1").is_err());
    assert!(carry_conversation(&a, &b, "abc-1").is_err());
    assert_eq!(std::fs::read_to_string(&on_to).unwrap(), "{}\n{\"more\":1}\n{\"pro\":1}\n", "untouched");
    // A directory that cannot be read is an error, never "nothing to carry" (which would
    // start a new conversation) nor "only the other copy" (an older one).
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(&on_from, "{}\n{\"more\":1}\n").unwrap();
    std::fs::write(&on_to, "{}\n").unwrap();
    for side in [from.join("projects"), to.join("projects")] {
        std::fs::set_permissions(&side, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read_dir(&side).is_err() {
            assert!(check_carry(&a, &b, "abc-1").is_err(), "{}", side.display());
            assert!(carry_conversation(&a, &b, "abc-1").is_err(), "{}", side.display());
        }
        std::fs::set_permissions(&side, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    assert_eq!(std::fs::read_to_string(&on_to).unwrap(), "{}\n", "untouched");
    // Accounts sharing their conversations see one file: nothing to do.
    let shared = dir.join("shared");
    std::fs::create_dir_all(shared.join("projects")).unwrap();
    std::os::unix::fs::symlink(from.join("projects"), shared.join("projects/../projects-link")).unwrap();
    let c = acct("claude", "shared", shared.to_str().unwrap());
    std::fs::remove_dir(shared.join("projects")).unwrap();
    std::fs::rename(shared.join("projects-link"), shared.join("projects")).unwrap();
    assert!(carry_conversation(&a, &c, "abc-1").unwrap());
    assert!(check_carry(&c, &a, "abc-1").is_ok());
    std::fs::set_permissions(&on_from, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(&on_from).is_err() {
        assert!(check_carry(&a, &c, "abc-1").is_err(), "even a shared transcript must be readable");
        std::fs::remove_file(&on_to).unwrap();
        assert!(check_carry(&a, &b, "abc-1").is_err(), "a first copy must be readable before stopping");
    }
    std::fs::set_permissions(&on_from, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::remove_file(&on_from).unwrap();
    std::fs::write(&on_to, "old copy\n").unwrap();
    assert!(check_carry(&a, &b, "abc-1").is_err(), "the destination alone cannot prove the source's history");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn what_the_screen_says_goes_up_or_down_and_ends() {
    let mut u = Usage::default();
    u.observe_screen(false, 100.0, 1_000);
    assert_eq!(u.peak(1_000), 100.0);
    u.observe_screen(false, 40.0, 2_000);
    assert_eq!(u.peak(2_000), 40.0, "a lower figure seen since counts");
    assert_eq!(u.five_hour.as_ref().unwrap().resets_at, Some(1_000 + 5 * 3600), "the same window");
    assert_eq!(u.peak(1_000 + 5 * 3600 + 1), 0.0, "the window ends with its period");
}

#[test]
fn removing_an_account_keeps_other_agents_rules_and_narrows() {
    let mut a = accounts();
    a.accounts.push(acct("codex", "pro", "/tmp/kraal-test-codex-pro"));
    a.rules.push(Rule {
        account_ids: None,
        workspace: Some("ops".into()),
        path: None,
        agent: Some("codex".into()),
        accounts: vec!["pro".into()],
    });
    assert!(a.remove("claude", "pro"));
    let usage = UsageMap::default();
    assert_eq!(a.choose("codex", Some("work"), None, &usage, 0).unwrap().name, "pro");
    // Claude used pro in "work": now no account there rather than any.
    assert!(a.choose("claude", Some("work"), None, &usage, 0).is_none());
    assert_eq!(a.choose("claude", None, None, &usage, 0).unwrap().name, "perso");
    assert!(a.remove("codex", "pro"));
    assert!(a.choose("codex", Some("ops"), None, &usage, 0).is_none(), "an emptied rule refuses");
}

#[test]
fn renaming_an_account_takes_its_rules_and_usage_along() {
    let dir = std::env::temp_dir().join(format!("kraal-rename-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (file, usage) = (dir.join("accounts.toml"), dir.join("usage"));
    std::fs::create_dir_all(&usage).unwrap();
    let rule = |workspace: &str, agent: Option<&str>, accounts: &[&str]| Rule {
        account_ids: None,
        workspace: Some(workspace.into()),
        path: None,
        agent: agent.map(String::from),
        accounts: accounts.iter().map(|n| n.to_string()).collect(),
    };
    let mut a = accounts();
    a.path = file.clone();
    a.accounts.push(acct("codex", "pro", "/tmp/kraal-test-codex-pro"));
    a.rules = vec![
        rule("work", None, &["pro"]),
        rule("ops", Some("claude"), &["pro", "perso"]),
        rule("ci", Some("codex"), &["pro"]),
        rule("home", None, &["perso"]),
    ];
    a.bind_rules();
    let mut rules = a.rules.clone();
    let pro_id = a.get("claude", "pro").unwrap().id;
    a.save().unwrap();
    std::fs::create_dir_all(usage.join("by-id")).unwrap();
    std::fs::write(usage.join("by-id").join(usage_file("claude", &pro_id)), "pro's").unwrap();
    // A removed account's usage, under the name perso takes.
    std::fs::write(usage.join("claude-me.json"), "stale").unwrap();

    Accounts::update_at(&file, |all| all.rename("claude", "pro", "work")).unwrap();
    Accounts::update_at(&file, |all| all.rename("claude", "perso", "me")).unwrap();
    let a = Accounts::load_from(&file).unwrap();
    let names: Vec<String> = a.all().iter().map(label).collect();
    assert_eq!(names, ["claude·work", "claude·me", "codex·pro"]);
    for (rule, names) in rules.iter_mut().zip([vec!["work", "pro"], vec!["work", "me"], vec!["pro"], vec!["me"]]) {
        rule.accounts = names.into_iter().map(String::from).collect();
    }
    assert_eq!(a.rules, rules);
    let none = UsageMap::default();
    assert_eq!(a.choose("claude", Some("work"), None, &none, 0).unwrap().name, "work");
    assert_eq!(a.choose("codex", Some("work"), None, &none, 0).unwrap().name, "pro");
    assert_eq!(std::fs::read_to_string(usage.join("by-id").join(format!("claude-{pro_id}.json"))).unwrap(), "pro's");
    assert_eq!(std::fs::read_to_string(usage.join("claude-me.json")).unwrap(), "stale", "unrelated cache is untouched");

    let text = std::fs::read_to_string(&file).unwrap();
    let refused = |agent: &str, old: &str, new: &str, why: &str| {
        let e = format!("{:#}", Accounts::update_at(&file, |all| all.rename(agent, old, new)).unwrap_err());
        assert!(e.contains(why), "{old} to {new}: {e}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), text, "{old} to {new} changed the file");
    };
    refused("claude", "work", "me", "already has an account named");
    refused("claude", "work", "my work", "invalid account name");
    refused("claude", "work", "default", "is the account in ~/.claude");
    refused("codex", "default", "mine", "check the existing sign-in");
    refused("claude", "gone", "here", "no account named");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_pane_follows_its_account_renamed() {
    let mut a = accounts();
    let env = |account: &Account| launch(account, &[], None, None).unwrap().env;
    let (pro, perso) = (env(&a.get("claude", "pro").unwrap()), env(&a.get("claude", "perso").unwrap()));
    a.rename("claude", "pro", "work").unwrap();
    a.rename("claude", "perso", "me").unwrap();
    assert_eq!(
        a.pane_account(&crate::pane::PaneSpec {
            account: Some("claude:pro".into()),
            env: pro.to_vec(),
            ..Default::default()
        })
        .unwrap()
        .name,
        "work"
    );
    assert_eq!(
        a.pane_account(&crate::pane::PaneSpec {
            account: Some("claude:perso".into()),
            env: perso.to_vec(),
            ..Default::default()
        })
        .unwrap()
        .name,
        "me",
        "in the default home"
    );
    assert_eq!(
        a.pane_account(&crate::pane::PaneSpec {
            account: Some("claude:work".into()),
            env: pro.to_vec(),
            ..Default::default()
        })
        .unwrap()
        .name,
        "work"
    );
    assert!(
        a.pane_account(&crate::pane::PaneSpec {
            account: Some("codex:gone".into()),
            env: vec![("CODEX_HOME".into(), "/tmp/kraal-test-nowhere".into())],
            ..Default::default()
        })
        .is_none()
    );
}

#[test]
fn path_rules() {
    let mut a = accounts();
    a.rules.push(Rule {
        account_ids: None,
        workspace: None,
        path: Some("/srv/client".into()),
        agent: Some("claude".into()),
        accounts: vec!["perso".into()],
    });
    let picked = a.candidates("claude", None, Some(Path::new("/srv/client/app")));
    assert_eq!(picked[0].name, "perso");
    assert_eq!(picked.len(), 1);
}

#[test]
fn launch_lines() {
    let a = accounts();
    let pro = a.get("claude", "pro").unwrap();
    let l = launch(&pro, &[], None, Some("abc")).unwrap();
    assert_eq!(l.argv, vec!["claude", "--session-id", "abc"]);
    assert!(l.env.contains(&("CLAUDE_CONFIG_DIR".into(), "/tmp/kraal-test-claude-pro".into())));
    assert!(l.env_remove.contains(&"ANTHROPIC_API_KEY".to_string()));
    let l = launch(&pro, &["--model".into(), "opus".into()], Some("s-1"), None).unwrap();
    assert_eq!(l.argv, vec!["claude", "--resume", "s-1", "--model", "opus"]);
    let perso = a.get("claude", "perso").unwrap();
    let l = launch(&perso, &[], None, None).unwrap();
    assert!(l.env_remove.contains(&"CLAUDE_CONFIG_DIR".to_string()), "default home clears the variable");
    let codex = Accounts::empty().get("codex", "default").unwrap();
    assert_eq!(launch(&codex, &[], Some("t-9"), None).unwrap().argv, vec!["codex", "resume", "t-9"]);
    assert_eq!(login(&pro).unwrap().argv, vec!["claude", "auth", "login"]);
}

#[test]
fn statusline_usage() {
    let v: serde_json::Value = serde_json::from_str(
            r#"{"session_id":"x","rate_limits":{"five_hour":{"used_percentage":42.5,"resets_at":1760000000},"seven_day":{"used_percentage":71}}}"#,
        )
        .unwrap();
    let u = usage_from_statusline(&v).unwrap();
    assert_eq!(u.five_hour.as_ref().unwrap().used_percent, 42.5);
    assert_eq!(u.weekly.as_ref().unwrap().used_percent, 71.0);
    assert!(usage_from_statusline(&serde_json::json!({"model":{}})).is_none());
}

#[test]
fn limits_without_a_reset_time_end() {
    let u = Usage {
        five_hour: Some(Window { used_percent: 100.0, resets_at: None }),
        observed_at: 1_000,
        ..Default::default()
    };
    assert_eq!(u.peak(1_000 + 3600), 100.0);
    assert_eq!(u.peak(1_000 + 5 * 3600 + 1), 0.0, "five hours later it has reset");
}

#[test]
fn usage_percentages_keep_missing_windows_and_expire_at_their_deadlines() {
    assert_eq!(Usage::default().percentages(1000).collect::<Vec<_>>(), [None, None]);
    for resets in [[None, None], [Some(19000), Some(605800)]] {
        let u = Usage {
            five_hour: Some(Window { used_percent: 95.0, resets_at: resets[0] }),
            weekly: Some(Window { used_percent: 71.0, resets_at: resets[1] }),
            observed_at: 1000,
            ..Default::default()
        };
        let saved = serde_json::to_vec(&u).unwrap();
        for (now, expected) in [
            (18999, [Some(95.0), Some(71.0)]),
            (19000, [Some(0.0), Some(71.0)]),
            (605799, [Some(0.0), Some(71.0)]),
            (605800, [Some(0.0), Some(0.0)]),
            (605801, [Some(0.0), Some(0.0)]),
            (900, [Some(95.0), Some(71.0)]),
        ] {
            assert_eq!(u.percentages(now).collect::<Vec<_>>(), expected, "{resets:?} at {now}");
        }
        assert_eq!(serde_json::to_vec(&u).unwrap(), saved);
    }
    let u = Usage {
        weekly: Some(Window { used_percent: 71.0, resets_at: None }),
        observed_at: i64::MAX - 1,
        ..Default::default()
    };
    assert_eq!(u.percentages(i64::MAX - 1).collect::<Vec<_>>(), [None, Some(71.0)]);
    assert_eq!(u.percentages(i64::MAX).collect::<Vec<_>>(), [None, Some(0.0)]);
}

/// What is worked out of the usage at one moment holds until the reset said to come next,
/// and no longer: frames that keep it until then show what a frame made afresh would.
#[test]
fn usage_holds_until_the_next_reset() {
    let week = 7 * 24 * 3600;
    let claude = Usage {
        five_hour: Some(Window { used_percent: 93.0, resets_at: Some(1_100) }),
        // It does not say when it resets: a week after it was seen.
        weekly: Some(Window { used_percent: 41.0, resets_at: None }),
        observed_at: 1_000,
        ..Default::default()
    };
    let codex = Usage {
        five_hour: Some(Window { used_percent: 58.0, resets_at: Some(1_250) }),
        observed_at: 1_000,
        ..Default::default()
    };
    let all = UsageMap(HashMap::from([("claude:default".into(), claude.clone()), ("codex:default".into(), codex)]));
    let peaks = |now| -> Vec<f64> { ["claude:default", "codex:default"].map(|k| all.0[k].peak(now)).to_vec() };
    let resets = [1_100, 1_250, 1_000 + week];
    let mut from = 1_000;
    for reset in resets {
        assert_eq!(all.next_end(from), Some(reset));
        for now in [from, (from + reset) / 2, reset - 1] {
            assert_eq!((peaks(now), all.next_end(now)), (peaks(from), Some(reset)), "at {now}");
        }
        assert_ne!(peaks(reset), peaks(reset - 1), "at {reset}");
        from = reset;
    }
    assert_eq!((peaks(from), all.next_end(from)), (vec![0.0, 0.0], None));
    assert_eq!((claude.peak(1_099), claude.peak(1_100), claude.next_end(1_100)), (93.0, 41.0, Some(1_000 + week)));
}

#[test]
fn screen_limits() {
    assert_eq!(limit_from_screen("…\nYou've hit your session limit · resets 3:45pm\n"), Some((false, 100.0)));
    assert_eq!(limit_from_screen("You've used 85% of your weekly limit · resets Mon"), Some((true, 85.0)));
    assert_eq!(limit_from_screen("all good"), None);
}

#[test]
fn screen_limits_are_the_agents_own_messages() {
    assert_eq!(
        limit_from_screen("  ⎿  Claude usage limit reached. Your limit will reset at 3pm"),
        Some((false, 100.0))
    );
    assert_eq!(limit_from_screen("5-hour limit reached ∙ resets 3am"), Some((false, 100.0)));
    assert_eq!(limit_from_screen("■ You've hit your usage limit. Try again in 2 hours."), Some((false, 100.0)));
    for quoted in [
        "> what should kraal do when you've hit your usage limit?",
        "│ > You've used 85% of your weekly limit, what now?",
        "I see: once you've hit your session limit, kraal changes account.",
        "+/// Limit messages printed by agents, e.g. \"You've hit your session limit · resets 3:45pm\"",
    ] {
        assert_eq!(limit_from_screen(quoted), None, "{quoted}");
    }
}

#[test]
fn reused_names_and_homes_do_not_capture_panes_rules_or_late_login_results() {
    let mut all = accounts();
    let old = all.get("claude", "pro").unwrap();
    let mut pane = crate::pane::PaneSpec {
        account: Some(key(&old.agent, &old.name)),
        env: launch(&old, &[], None, None).unwrap().env,
        ..Default::default()
    };
    // A saved pane from 0.2 or 0.3 has no identity, even in its environment.
    pane.env.retain(|(k, _)| k != "KRAAL_ACCOUNT_ID");
    all.bind_pane(&mut pane);
    assert_eq!(pane.account_id.as_deref(), Some(old.id.as_str()));
    all.rename("claude", "pro", "work").unwrap();
    all.bind_pane(&mut pane);
    assert_eq!(pane.account.as_deref(), Some("claude:work"));
    let replacement = Account { id: new_uuid(), home: "/replacement".into(), sign_in_required: true, ..old.clone() };
    all.add(replacement.clone()).unwrap();
    assert_eq!(all.pane_account(&pane).unwrap().id, old.id);
    all.remove("claude", "work");
    all.bind_pane(&mut pane);
    assert_eq!(pane.account.as_deref(), Some("claude:work"), "the removed account remains readable");
    assert_eq!(pane.account_id.as_deref(), Some(old.id.as_str()));
    assert!(all.by_id(&old.id).is_none());
    assert!(all.pane_account(&pane).is_none());
    assert!(all.choose("claude", Some("work"), None, &UsageMap::default(), 0).is_none());
    all.set_sign_in_required(&old, false);
    assert!(all.get("claude", "pro").unwrap().sign_in_required, "a late login belongs to the removed identity");
}

#[test]
fn a_dangling_legacy_pane_never_binds_to_a_later_account() {
    let mut all = Accounts::empty();
    let mut spec = crate::pane::PaneSpec {
        account: Some("claude:gone".into()),
        env: vec![("CLAUDE_CONFIG_DIR".into(), "/gone".into())],
        ..Default::default()
    };
    all.bind_pane(&mut spec);
    let id = spec.account_id.clone();
    all.add(acct("claude", "gone", "/gone")).unwrap();
    all.bind_pane(&mut spec);
    assert_eq!(spec.account_id, id);
    assert!(all.by_id(id.as_deref().unwrap()).is_none());
}

#[test]
fn invalid_account_identities_leave_the_file_untouched() {
    let dir = std::env::temp_dir().join(format!("kraal-account-bad-id-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("accounts.toml");
    for source in [
        "[[account]]\nid = '../bad'\nname = 'a'\nagent = 'claude'\nhome = '/a'\n".to_string(),
        "[[account]]\nid = 'default-claude'\nname = 'a'\nagent = 'claude'\nhome = '/a'\n".to_string(),
        ["a", "b"]
            .map(|name| format!("[[account]]\nid = 'same'\nname = '{name}'\nagent = 'claude'\nhome = '/{name}'\n"))
            .concat(),
    ] {
        std::fs::write(&path, &source).unwrap();
        assert!(
            Accounts::update_at(&path, |all| {
                all.limits.threshold = 80;
                Ok(())
            })
            .is_err()
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
        assert!(!path.with_extension("toml.kraal-bak").exists());
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn naming_the_default_home_keeps_its_identity_and_bindings() {
    let dir = std::env::temp_dir().join(format!("kraal-adopt-default-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("accounts.toml");
    std::fs::write(&file, "[[rule]]\nworkspace = 'w'\naccounts = ['default']\nagent = 'claude'\n").unwrap();
    let mut all = Accounts::load_from(&file).unwrap();
    let old = all.get("claude", "default").unwrap();
    let mut pane = crate::pane::PaneSpec {
        account: Some(key(&old.agent, &old.name)),
        env: launch(&old, &[], None, None).unwrap().env,
        ..Default::default()
    };
    all.bind_pane(&mut pane);
    let definition = dir.join("w.kdl");
    std::fs::write(&definition, "layout { pane agent=\"claude:default\" }\n").unwrap();
    let source = crate::workspace::bind_source_accounts("layout { pane agent=\"claude:default\" }\n", &all).unwrap();
    std::fs::write(&definition, source).unwrap();
    std::fs::create_dir_all(dir.join("by-id")).unwrap();
    std::fs::write(
        dir.join("by-id").join(usage_file("claude", &old.id)),
        serde_json::to_vec(&Usage {
            five_hour: Some(Window { used_percent: 42.0, resets_at: Some(i64::MAX) }),
            ..Default::default()
        })
        .unwrap(),
    )
    .unwrap();
    let adopted = all.add(Account { id: new_uuid(), name: "personal".into(), ..old.clone() }).unwrap();
    assert_eq!(adopted.id, old.id, "callers get the adopted identity too");
    assert_eq!(all.for_agent("claude"), std::slice::from_ref(&adopted));
    let legacy = all.accounts.iter().find(|a| a.name == "default").unwrap();
    assert_eq!(legacy.linked_to.as_deref(), Some(adopted.id.as_str()));
    assert!(legacy.same_home(&adopted));
    all.bind_pane(&mut pane);
    assert_eq!(pane.account.as_deref(), Some("claude:personal"));
    assert_eq!(pane.account_id.as_deref(), Some(old.id.as_str()));
    assert_eq!(all.pane_account(&pane), Some(adopted.clone()));
    let def = crate::workspace::load_file(&definition).unwrap();
    assert_eq!(all.by_id(def.panes()[0].account_id.as_deref().unwrap()), Some(adopted.clone()));
    let usage: Usage =
        serde_json::from_slice(&std::fs::read(dir.join("by-id").join(usage_file("claude", &adopted.id))).unwrap())
            .unwrap();
    let usage = UsageMap(HashMap::from([(adopted.key(), usage)]));
    assert_eq!(usage.peak(&adopted, 0), Some(42.0));
    assert_eq!(all.choose("claude", Some("w"), None, &usage, 0), Some(adopted.clone()));
    all.bind_rules();
    all.save().unwrap();
    assert_eq!(Accounts::load_from(&file).unwrap().get("claude", "personal"), Some(adopted));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn save_and_reload() {
    let dir = std::env::temp_dir().join(format!("kraal-accounts-{}", std::process::id()));
    let path = dir.join("accounts.toml");
    let mut a = Accounts::load_from(&path).unwrap();
    a.add(acct("codex", "work", "/tmp/x")).unwrap();
    assert!(a.add(acct("codex", "work", "/tmp/y")).is_err());
    assert!(a.add(acct("nope", "w", "/tmp/z")).is_err());
    a.limits.mode = Mode::Ask;
    a.save().unwrap();
    let b = Accounts::load_from(&path).unwrap();
    assert_eq!(b.limits.mode, Mode::Ask);
    assert_eq!(b.accounts.len(), 1);
    let _ = std::fs::remove_dir_all(dir);
}

/// A user's own file: a header, comments, an order that is not kraal's, and keys only another
/// kraal reads.
const BY_HAND: &str = "\
# The accounts of the team laptop.

[limits]
mode = \"ask\"          # never move by itself
threshold = 85

# Paid by work.
[[account]]
agent = \"claude\"
name = \"pro\"
home = \"/tmp/kraal-test-claude-pro\"
seat = 4              # read by a newer kraal
id = \"legacy-67bae7cfc475c28e\"

[[account]]
name = \"pro\"
agent = \"codex\"
home = \"/tmp/kraal-test-codex-pro\"
id = \"legacy-e51c15a4282e68d8\"

# Work first, then mine.
[[rule]]
workspace = \"work\"
accounts = [\"pro\", \"default\"]
account_ids = [\"claude:@legacy-67bae7cfc475c28e\", \"codex:@legacy-e51c15a4282e68d8\", \"claude:@default-claude\", \"codex:@default-codex\"]
until = 2027-01-01    # read by a newer kraal
";

/// What a file that held `BY_HAND` holds once `change` was made to the accounts.
fn changed(name: &str, change: impl FnOnce(&mut Accounts) -> Result<()>) -> String {
    changed_text(name, BY_HAND, change)
}

pub(super) fn changed_text(name: &str, text: &str, change: impl FnOnce(&mut Accounts) -> Result<()>) -> String {
    let dir = std::env::temp_dir().join(format!("kraal-accounts-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("accounts.toml");
    std::fs::write(&path, text).unwrap();
    Accounts::update_at(&path, change).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    let _ = std::fs::remove_dir_all(dir);
    text
}

#[test]
fn a_change_leaves_the_rest_of_the_file_as_the_user_wrote_it() {
    assert_eq!(changed("same", |_| Ok(())), BY_HAND);
    // `kraal accounts limits`: the value changes where it is.
    let text = changed("limits", |all| {
        all.limits.threshold = 70;
        all.limits.mode = Mode::Auto;
        Ok(())
    });
    let expected = BY_HAND.replace("mode = \"ask\"", "mode = \"auto\"").replace("threshold = 85", "threshold = 70");
    assert_eq!(text, expected);
    // Installing the hooks: a table the file did not have comes last.
    let text = changed("statusline", |all| {
        all.claude.statusline = Some("my-line".into());
        Ok(())
    });
    assert_eq!(text, format!("{BY_HAND}\n[claude]\nstatusline = \"my-line\"\n"));
    // `kraal accounts add`: one more account, after the others.
    let text = changed("add", |all| all.add(acct("claude", "perso", "/tmp/kraal-test-claude-perso")).map(|_| ()));
    let added = format!(
        "[[account]]\nid = {:?}\nname = \"perso\"\nagent = \"claude\"\nhome = \"/tmp/kraal-test-claude-perso\"\n",
        acct("claude", "perso", "/tmp/kraal-test-claude-perso").id
    );
    assert_eq!(text, BY_HAND.replace("\n# Work first", &format!("\n{added}\n# Work first")));
    // `kraal accounts rm`: the account goes with the comment about it, the header stays.
    let text = changed("rm", |all| {
        all.remove("claude", "pro");
        Ok(())
    });
    assert_eq!(
        text,
        BY_HAND
            .replace(&BY_HAND[BY_HAND.find("# Paid").unwrap()..BY_HAND.find("[[account]]\nname").unwrap()], "")
            .replace(
                "accounts = [\"pro\", \"default\"]",
                "accounts = [\"claude:@legacy-67bae7cfc475c28e\",\"pro\", \"default\"]"
            )
    );
    // Renaming changes only the label; the rule keeps its identity.
    let text = changed("rename", |all| all.rename("claude", "pro", "work"));
    assert_eq!(
        text,
        BY_HAND
            .replace(
                "name = \"pro\"\nhome = \"/tmp/kraal-test-claude-pro\"",
                "name = \"work\"\nhome = \"/tmp/kraal-test-claude-pro\""
            )
            .replace("accounts = [\"pro\", \"default\"]", "accounts = [\"work\",\"pro\", \"default\"]")
    );
}

#[test]
fn claude_settings_and_existing_rules_keep_their_spelling_and_order() {
    for (i, claude) in [
        "[claude]\nstatusline = 'my-line' # run mine too\n",
        "[claude]\nstatusline = '' # still a command value\n",
        "[claude]\nfuture = 'kept'\n",
        "[claude]\n[claude.future]\ncolor = 'blue'\n",
        "claude = { future = 'kept' }\n",
        "claude = {}\n",
        "[claude]\nrule = [] # a key this kraal does not read\n",
        "[claude]\nstatusline = '''\nrule = []\n[claude]\n'''\n",
    ]
    .iter()
    .enumerate()
    {
        let old = format!("{claude}{BY_HAND}");
        let text = changed_text(&format!("claude-{i}"), &old, |all| {
            all.limits.threshold = 70;
            Ok(())
        });
        assert_eq!(text, old.replace("threshold = 85", "threshold = 70"));
    }
    let rule = BY_HAND.lines().find(|line| line.starts_with("accounts = ")).unwrap();
    let ids = BY_HAND.lines().find(|line| line.starts_with("account_ids = ")).unwrap();
    let old = BY_HAND.replace(rule, "accounts = []").replace(ids, "account_ids = []");
    assert_eq!(changed_text("rule-no-accounts", &old, |all| all.save()), old);
}

#[test]
fn setting_and_clearing_a_statusline_keeps_unknown_claude_settings() {
    for (i, settings) in ["", "future = 'kept'\n"].iter().enumerate() {
        let old = format!("# My status line.\n[claude] # integration\n{settings}\n{BY_HAND}");
        let text = changed_text(&format!("set-statusline-{i}"), &old, |all| {
            all.claude.statusline = Some("my-line".into());
            Ok(())
        });
        let expected =
            old.replace(&format!("{settings}\n{BY_HAND}"), &format!("{settings}statusline = \"my-line\"\n\n{BY_HAND}"));
        assert_eq!(text, expected);
        let text = changed_text(&format!("clear-statusline-{i}"), &text, |all| {
            all.claude.statusline = None;
            Ok(())
        });
        assert_eq!(text, old);
    }
}

#[test]
fn a_new_file_is_written_whole_and_one_that_cannot_be_read_is_left_alone() {
    let dir = std::env::temp_dir().join(format!("kraal-accounts-new-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("accounts.toml");
    let add = |all: &mut Accounts| all.add(acct("codex", "work", "/tmp/kraal-test-codex-work"));
    Accounts::update_at(&path, add).unwrap();
    let whole = "[limits]\nthreshold = 90\nmode = \"notify\"\n\n\
                     [[account]]\nname = \"work\"\nagent = \"codex\"\nhome = \"/tmp/kraal-test-codex-work\"\n";
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        format!("{HEADER}{whole}").replace(
            "[[account]]\n",
            &format!("[[account]]\nid = {:?}\n", acct("codex", "work", "/tmp/kraal-test-codex-work").id)
        )
    );
    // The file `kraal accounts edit` makes when there is none takes its first account
    // under its header.
    std::fs::remove_file(&path).unwrap();
    Accounts::load_from(&path).unwrap().save().unwrap();
    let empty = std::fs::read_to_string(&path).unwrap();
    assert!(empty.starts_with(HEADER) && !empty.contains("rule = []") && !empty.contains("\n[claude]"), "{empty}");
    Accounts::update_at(&path, add).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.starts_with(HEADER) && text.contains("name = \"work\"\n"), "{text}");
    assert_eq!(Accounts::load_from(&path).unwrap().accounts, [acct("codex", "work", "/tmp/kraal-test-codex-work")]);

    for broken in
        ["# mine\nrule = []\n[claude]\n[[account]\nname = \"work\"\n", "# mine\nrule = []\n[claude]\nstatusline = 42\n"]
    {
        std::fs::write(&path, broken).unwrap();
        assert!(Accounts::update_at(&path, add).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn keys_the_accounts_file_does_not_read_are_warnings() {
    let dir = std::env::temp_dir().join(format!("kraal-accounts-unknown-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("accounts.toml");
    std::fs::write(
        &path,
        "[limits]\ntreshold = 80\n\n[[account]]\nid = \"pro\"\nname = \"pro\"\nagent = \"claude\"\nhome = \"/tmp/x\"\n\
             notes = \"team plan\"\n\n[[rule]]\nworkspce = \"shop\"\naccounts = [\"pro\"]\n",
    )
    .unwrap();
    let (a, warnings) = Accounts::read(&path).unwrap();
    assert_eq!(
        warnings,
        [
            "account[0].notes: unknown key (did you mean note?)",
            "limits.treshold: unknown key (did you mean threshold?)",
            "rule[0].workspce: unknown key (did you mean workspace?)",
        ]
    );
    assert_eq!((a.limits.threshold, a.accounts.len(), a.rules[0].workspace.as_deref()), (90, 1, None));
    // What kraal writes, every field set, it reads.
    let mut full = Accounts::empty();
    full.path = path.clone();
    full.limits = Limits { threshold: 80, mode: Mode::Auto };
    full.claude.statusline = Some("my-statusline".into());
    full.accounts =
        vec![Account { threshold: Some(70), note: Some("team plan".into()), ..acct("claude", "pro", "/tmp/x") }];
    full.rules = vec![Rule {
        account_ids: None,
        workspace: Some("shop".into()),
        path: Some("~/src/shop".into()),
        agent: Some("claude".into()),
        accounts: vec!["pro".into()],
    }];
    full.bind_rules();
    full.save().unwrap();
    let (again, warnings) = Accounts::read(&path).unwrap();
    assert_eq!(warnings, [] as [&str; 0]);
    assert_eq!(
        (again.accounts, again.rules, again.claude.statusline),
        (full.accounts, full.rules, full.claude.statusline)
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// An agent may say anything of when its limits reset, and a usage file anything of when it was
/// seen: nothing overflows working out how long that is.
#[test]
fn usage_any_time_says_is_shown() {
    let now = 1_700_000_000;
    for at in [i64::MIN, i64::MAX, -1] {
        assert!(describe_reset(Some(at), now).starts_with("resets in"));
        assert!(describe_reset(Some(now), at).starts_with("resets in"));
        let usage = Usage {
            five_hour: Some(Window { used_percent: 10.0, resets_at: None }),
            weekly: None,
            observed_at: at,
            source: String::new(),
        };
        let _ = usage.peak(now);
    }
}

/// What agents say of their usage (Claude Code's status line payload, the usage files kraal keeps,
/// limit messages on a screen), any of it: read and shown without panicking.
#[test]
fn fuzz_accounts_usage() {
    crate::fuzz::run("accounts_usage", 300, |rng| {
        let number = |rng: &mut crate::fuzz::Rng| -> String {
            match rng.below(5) {
                0 => (rng.number() as i64).to_string(),
                1 => format!("{}.5", rng.number() as i64),
                2 => format!("{:e}", rng.number() as f64 * 1e300),
                3 => "null".into(),
                _ => (rng.below(200) as f64 - 50.0).to_string(),
            }
        };
        let window = |rng: &mut crate::fuzz::Rng| -> String {
            format!("{{\"used_percentage\":{},\"resets_at\":{}}}", number(rng), number(rng))
        };
        let (a, b) = (window(rng), window(rng));
        let mut payload = format!("{{\"rate_limits\":{{\"five_hour\":{a},\"seven_day\":{b}}}}}").into_bytes();
        if rng.one_in(3) {
            rng.mutate(&mut payload, &[b"{", b"}", b":", b",", b"\"", b"9e999", b"-"]);
        }
        let now = rng.number() as i64 >> rng.below(40);
        let value: serde_json::Value = serde_json::from_slice(&payload).unwrap_or_default();
        if let Some(usage) = usage_from_statusline(&value) {
            let _ = usage.peak(now);
            let _ = describe_reset(usage.next_reset(now), now);
            for w in usage.five_hour.iter().chain(usage.weekly.iter()) {
                let _ = describe_reset(w.resets_at, now);
            }
            let _ = crate::accounts::usage_text(&usage, now);
        }
        // A usage file, as kraal wrote it or damaged.
        let mut saved = serde_json::to_vec(&Usage {
            five_hour: Some(Window { used_percent: 50.0, resets_at: Some(rng.number() as i64) }),
            weekly: None,
            observed_at: rng.number() as i64,
            source: "statusline".into(),
        })
        .unwrap();
        rng.mutate(&mut saved, &[b"9223372036854775807", b"-9223372036854775808", b"null"]);
        if let Ok(mut usage) = serde_json::from_slice::<Usage>(&saved) {
            let _ = usage.peak(now);
            let _ = usage.next_reset(now);
            usage.observe_screen(rng.one_in(2), 85.0, now);
            let _ = crate::accounts::usage_text(&usage, now);
        }
        let screen = String::from_utf8_lossy(&rng.text(400)).into_owned()
            + *rng.pick(&["You've used 85% of your weekly limit", "used 1e309% of your session limit", "used -5%"]);
        let _ = limit_from_screen(&screen);
    });
}
#[test]
fn a_process_account_uses_its_own_home_and_refuses_credential_overrides() {
    let dir = std::env::temp_dir().join(format!("kraal-process-account-{}", std::process::id()));
    std::fs::create_dir_all(dir.join(".claude")).unwrap();
    let mut all = Accounts::empty();
    let mut a = Accounts::standard("claude").unwrap();
    a.name = "personal".into();
    a.id = new_uuid();
    a.home = dir.join(".claude").display().to_string();
    all.add(a.clone()).unwrap();
    let kind = agent_kind("claude").unwrap();
    let mut env = HashMap::from([("HOME".into(), dir.display().to_string())]);
    assert_eq!(all.process_account(kind, &env).unwrap().id, a.id);
    env.insert("HOME".into(), "/unrelated-home".into());
    assert!(all.process_account(kind, &env).is_none());
    env.insert("CLAUDE_CONFIG_DIR".into(), a.home.clone());
    assert_eq!(all.process_account(kind, &env).unwrap().id, a.id);
    for key in kind.clear_env {
        env.insert((*key).into(), "override".into());
        assert!(all.process_account(kind, &env).is_none(), "{key}");
        env.remove(*key);
    }
    env.insert("CLAUDE_CONFIG_DIR".into(), "relative".into());
    assert!(all.process_account(kind, &env).is_none());
    assert!(all.process_account(kind, &HashMap::new()).is_none());
    let target = all.add(acct("claude", "work", "/work")).unwrap();
    all.accounts.iter_mut().find(|b| b.id == a.id).unwrap().review = true;
    all.review(&a, Some(&target)).unwrap();
    env.insert("CLAUDE_CONFIG_DIR".into(), a.home.clone());
    let joined = all.process_account(kind, &env).unwrap();
    assert_eq!(joined.id, target.id);
    assert_eq!(joined.home, a.home, "a process in a joined home keeps its launch home");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn codex_resume_requires_readable_matching_session_metadata() {
    let dir = std::env::temp_dir().join(format!("kraal-codex-resume-{}", std::process::id()));
    let mut a = Accounts::standard("codex").unwrap();
    a.home = dir.display().to_string();
    let folder = dir.join("sessions/2026/10/13");
    std::fs::create_dir_all(&folder).unwrap();
    let file = folder.join("rollout-today-original.jsonl");
    assert!(check_resume(&a, "original").is_err());
    for text in ["", "not json", r#"{"type":"session_meta","payload":{"id":"different"}}"#] {
        std::fs::write(&file, text).unwrap();
        assert!(check_resume(&a, "original").is_err());
    }
    std::fs::write(&file, "{\"type\":\"session_meta\",\"payload\":{\"id\":\"original\"}}\n").unwrap();
    check_resume(&a, "original").unwrap();
    assert!(check_resume(&a, "../original").is_err());
    assert!(check_resume(&a, "").is_err());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn old_account_files_are_refused_without_rewriting() {
    let dir = std::env::temp_dir().join(format!("kraal-old-accounts-{}", new_uuid()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("accounts.toml");
    for source in [
        "[[account]]\nagent = 'claude'\nname = 'work'\nhome = '/work'\n",
        "[[account]]\nid = 'owner'\nagent = 'claude'\nname = 'work'\nhome = '/work'\n[[account.references]]\nagent = 'claude'\nname = 'old'\nhome = '/old'\n",
        "[[forgotten]]\nagent = 'claude'\nname = 'old'\nhome = '/old'\n",
    ] {
        std::fs::write(&path, source).unwrap();
        assert!(Accounts::load_from(&path).unwrap_err().to_string().ends_with("run kraal 0.4 once first"));
        assert!(Accounts::check(&path).unwrap_err().to_string().ends_with("run kraal 0.4 once first"));
        let error = Accounts::update_at::<()>(&path, |_| panic!("refuse before changing accounts")).unwrap_err();
        assert!(error.to_string().ends_with("run kraal 0.4 once first"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
        assert!(!path.with_extension("toml.kraal-bak").exists());
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn usage_reads_only_identity_caches_and_leaves_name_caches_untouched() {
    let dir = std::env::temp_dir().join(format!("kraal-old-usage-{}", new_uuid()));
    std::fs::create_dir_all(&dir).unwrap();
    let all = accounts();
    let account = all.get("claude", "pro").unwrap();
    let bytes = serde_json::to_vec(&Usage { observed_at: 20, ..Default::default() }).unwrap();
    let legacy = dir.join(usage_file("claude", "pro"));
    std::fs::write(&legacy, &bytes).unwrap();
    assert!(UsageMap::load_from(&all, &dir).get(&account).is_none());
    assert!(!dir.join("by-id").exists());
    std::fs::create_dir_all(dir.join("by-id")).unwrap();
    std::fs::write(dir.join("by-id").join(usage_file("claude", &account.id)), &bytes).unwrap();
    assert_eq!(UsageMap::load_from(&all, &dir).get(&account).unwrap().observed_at, 20);
    assert_eq!(std::fs::read(&legacy).unwrap(), bytes);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn an_older_usage_observation_does_not_replace_a_newer_one() {
    let account = Account::new("codex", "poll", "/unused/codex-poll".into(), None, false);
    let saved = || {
        let file = paths::state_dir().join("usage/by-id").join(usage_file("codex", &account.id));
        serde_json::from_slice::<Usage>(&std::fs::read(file).unwrap()).unwrap().observed_at
    };
    save_usage(&account, &Usage { observed_at: 200, ..Default::default() }).unwrap();
    save_usage(&account, &Usage { observed_at: 100, ..Default::default() }).unwrap();
    assert_eq!(saved(), 200);
    save_usage(&account, &Usage { observed_at: 300, ..Default::default() }).unwrap();
    assert_eq!(saved(), 300);
}

#[test]
fn pane_resolution_prioritizes_opaque_ids_without_reusing_names_or_homes() {
    let mut all = accounts();
    let old = all.get("claude", "pro").unwrap();
    let other = all.get("claude", "perso").unwrap();
    all.rename("claude", "pro", "work").unwrap();
    for (id, env_id, reference, expected) in [
        (Some(old.id.as_str()), Some(other.id.as_str()), "claude:perso", Some(old.id.as_str())),
        (Some("removed-id"), Some(old.id.as_str()), "claude:work", None),
        (None, Some(old.id.as_str()), "claude:perso", Some(old.id.as_str())),
        (None, Some("removed-id"), "claude:work", None),
        (None, None, old.key().as_str(), Some(old.id.as_str())),
        (None, None, "claude:@removed-id", None),
        (None, None, "claude:pro", Some(old.id.as_str())),
    ] {
        let mut pane = crate::pane::PaneSpec {
            account: Some(reference.into()),
            account_id: id.map(String::from),
            env: vec![("CLAUDE_CONFIG_DIR".into(), old.home.clone())],
            ..Default::default()
        };
        if let Some(id) = env_id {
            pane.env.push(("KRAAL_ACCOUNT_ID".into(), id.into()));
        }
        let home = pane.env[0].clone();
        assert_eq!(all.pane_account(&pane).as_ref().map(|a| a.id.as_str()), expected);
        all.bind_pane(&mut pane);
        assert_eq!(pane.env[0], home, "refreshing the label preserves the launch home");
        assert_eq!(all.pane_account(&pane).as_ref().map(|a| a.id.as_str()), expected);
        if expected.is_some() {
            assert_eq!(pane.account.as_deref(), Some("claude:work"));
        }
    }
    let identity_only = crate::pane::PaneSpec { account_id: Some(old.id.clone()), ..Default::default() };
    assert_eq!(all.pane_account(&identity_only).unwrap().id, old.id);
    let mut unlabelled = identity_only.clone();
    all.bind_pane(&mut unlabelled);
    assert!(unlabelled.account.is_none());
    let environment_only =
        crate::pane::PaneSpec { env: vec![("KRAAL_ACCOUNT_ID".into(), old.id.clone())], ..Default::default() };
    assert!(all.pane_account(&environment_only).is_none());
    let mut pane = crate::pane::PaneSpec {
        account: Some("claude:work".into()),
        account_id: Some(old.id.clone()),
        env: vec![("CLAUDE_CONFIG_DIR".into(), old.home.clone())],
        ..Default::default()
    };
    all.remove("claude", "work");
    all.add(Account { id: "replacement-id".into(), name: "work".into(), ..old }).unwrap();
    all.bind_pane(&mut pane);
    assert!(all.pane_account(&pane).is_none());
}

#[test]
fn prepared_add_revalidates_before_creating_a_home_and_keeps_verified_homes() {
    if !crate::testing::in_own_process(
        "accounts::tests::prepared_add_revalidates_before_creating_a_home_and_keeps_verified_homes",
    ) {
        return;
    }
    let dir = crate::testing::TempDir::new("prepared-add");
    let path = dir.0.join("accounts.toml");
    let new = Account::new("codex", "work", dir.0.join("work").display().to_string(), Some(80), true);
    let wrong = Account { id: "another-identity".into(), ..new.clone() };
    let error = Accounts::update_at(&path, |all| all.add_prepared(new.clone(), Some((&wrong, false)))).unwrap_err();
    assert_eq!(error.to_string(), "the account changed while checking sign-in; run the add command again");
    assert!(!dir.0.join("work").exists());
    assert!(!path.exists());

    let (added, _) = Accounts::update_at(&path, |all| all.add_prepared(new.clone(), None)).unwrap();
    assert_eq!(added, new);
    assert!(dir.0.join("work").is_dir());
    assert_eq!(Accounts::load_from(&path).unwrap().by_id(&new.id), Some(new.clone()));

    let alias = Account::new("codex", "alias", new.home.clone(), None, true);
    let (verified, notes) = Accounts::update_at(&path, |all| all.add_prepared(alias, Some((&new, true)))).unwrap();
    assert_eq!(verified.id, new.id);
    assert!(!verified.sign_in_required);
    assert!(notes.is_empty());
    assert!(!Accounts::load_from(&path).unwrap().by_id(&new.id).unwrap().sign_in_required);

    let untouched = Account::new("codex", "verified", dir.0.join("untouched").display().to_string(), None, false);
    Accounts::update_at(&path, |all| all.add_prepared(untouched.clone(), Some((&untouched, true)))).unwrap();
    assert!(!dir.0.join("untouched").exists(), "verified homes need no preparation");
}
