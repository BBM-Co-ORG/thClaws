//! `thclaws agent new` — scaffold a best-practice agent skeleton (dev-plan/48.6).
//!
//! Writes a folder that `thclaws agent validate` passes out of the box: a
//! role-scoped subagent set (planner + worker + a READ-ONLY verifier) with
//! `output_schema` files, a manifest pinned to the supporting engine version,
//! and — for the static / batch-fanout patterns — a deterministic
//! `WorkflowRun` script with the bounded-loop + graceful-`step()` + `thclaws.log`
//! \+ verifier-gate shape. The conversational meta-agent `agent-builder` is the
//! guided experience; this is the zero-LLM deterministic skeleton it (or a human)
//! starts from.

use std::path::Path;

/// The three starter shapes.
pub const PATTERNS: &[&str] = &["static-pipeline", "batch-fanout", "dynamic"];

fn write(files: &mut Vec<String>, dir: &Path, rel: &str, body: &str) -> Result<(), String> {
    let p = dir.join(rel);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    std::fs::write(&p, body).map_err(|e| format!("write {}: {e}", p.display()))?;
    files.push(rel.to_string());
    Ok(())
}

/// Scaffold an agent at `dir`. `name` defaults to the dir's file name.
/// Returns the relative paths written. Refuses a non-empty `dir` unless `force`.
pub fn scaffold_agent(
    dir: &Path,
    pattern: &str,
    name: Option<&str>,
    force: bool,
) -> Result<Vec<String>, String> {
    if !PATTERNS.contains(&pattern) {
        return Err(format!(
            "unknown pattern '{pattern}' — expected one of: {}",
            PATTERNS.join(" | ")
        ));
    }
    if dir.exists()
        && dir
            .read_dir()
            .map(|mut d| d.next().is_some())
            .unwrap_or(false)
        && !force
    {
        return Err(format!(
            "{} is not empty — pass --force to scaffold into it",
            dir.display()
        ));
    }
    let slug = name
        .map(|s| s.to_string())
        .or_else(|| dir.file_name().and_then(|s| s.to_str()).map(String::from))
        .unwrap_or_else(|| "my-agent".to_string());
    let static_like = pattern != "dynamic";
    let mut files = Vec::new();

    // ── manifest + identity ────────────────────────────────────────────────
    write(
        &mut files,
        dir,
        "manifest.json",
        r#"{
  "version": "0.1.0",
  "categories": ["custom"],
  "license": "MIT",
  "requires": {
    "thclaws_min_version": "0.73.0",
    "mcp_servers": []
  },
  "permissions": {
    "shell_execution": "sandboxed"
  }
}
"#,
    )?;
    write(
        &mut files,
        dir,
        ".thclaws/settings.json",
        &format!(
            r#"{{
  "agent": {{
    "id": "{slug}",
    "name": "{slug}",
    "description": "A {pattern} agent scaffolded with `thclaws agent new`. Replace this description, AGENTS.md, and the role subagents with your own."
  }}
}}
"#
        ),
    )?;

    // ── role subagents + schemas ───────────────────────────────────────────
    write(
        &mut files,
        dir,
        ".thclaws/schemas/planner.json",
        "{\n  \"type\": \"object\",\n  \"required\": [\"steps\"],\n  \"properties\": {\n    \"goal\": { \"type\": \"string\" },\n    \"steps\": { \"type\": \"array\", \"items\": { \"type\": \"object\", \"required\": [\"title\"], \"properties\": { \"title\": { \"type\": \"string\" } } } }\n  }\n}\n",
    )?;
    write(
        &mut files,
        dir,
        ".thclaws/schemas/worker.json",
        "{\n  \"type\": \"object\",\n  \"required\": [\"status\"],\n  \"properties\": {\n    \"status\": { \"type\": \"string\" },\n    \"output\": { \"type\": \"string\" }\n  }\n}\n",
    )?;
    write(
        &mut files,
        dir,
        ".thclaws/schemas/verifier.json",
        "{\n  \"type\": \"object\",\n  \"required\": [\"ok\"],\n  \"properties\": {\n    \"ok\": { \"type\": \"boolean\" },\n    \"errors\": { \"type\": \"array\" }\n  }\n}\n",
    )?;
    write(
        &mut files,
        dir,
        ".thclaws/agents/planner.md",
        "---\nname: planner\ndescription: Breaks the request into a small, ordered list of concrete steps. Read-only.\ntools: Read, Grep, Glob\noutput_schema: ../schemas/planner.json\ncolor: cyan\n---\n\n# Planner (sub-role)\n\nYou turn the request into a plan: the goal plus an ordered list of concrete\nsteps the worker can execute one at a time. You do NOT do the work and you do\nNOT write files.\n\nReturn ONLY JSON matching your `output_schema`: `{ \"goal\": \"…\", \"steps\": [ { \"title\": \"…\" } ] }`.\n",
    )?;
    write(
        &mut files,
        dir,
        ".thclaws/agents/worker.md",
        "---\nname: worker\ndescription: Executes ONE step and writes its output. Confined to writing inside output/.\ntools: Read, Write, Edit, Bash\nwritePaths: output/**\noutput_schema: ../schemas/worker.json\ncolor: green\n---\n\n# Worker (sub-role)\n\nYou execute exactly the one step the caller hands you and write any artifact\nunder `output/`. Match the existing project conventions; don't do the next\nstep. Replace this with your agent's real work (call the MCP/scripts that do\nthe generation, etc.).\n\nReturn ONLY JSON matching your `output_schema`: `{ \"status\": \"done\", \"output\": \"<path or note>\" }`.\n",
    )?;
    write(
        &mut files,
        dir,
        ".thclaws/agents/verifier.md",
        "---\nname: verifier\ndescription: The independent completion gate. Checks the result on disk and returns an objective pass/fail. Read-only + Bash — CANNOT write, so it can't green-wash its own result.\ntools: Read, Bash, Grep\noutput_schema: ../schemas/verifier.json\ncolor: red\n---\n\n# Verifier (sub-role)\n\nYou are the proof-of-done gate. You have NO Write tool — you check the\nartifacts on disk (and run any deterministic check the agent ships) and report\nhonestly. Never report `ok: true` unless it genuinely is.\n\nReturn ONLY JSON matching your `output_schema`: `{ \"ok\": false, \"errors\": [\"…\"] }`.\n",
    )?;

    // ── AGENTS.md (orchestrator) ───────────────────────────────────────────
    let agents_md = if static_like {
        format!(
            "# {slug}\n\nScaffolded with `thclaws agent new --pattern {pattern}`. The work runs as a\ndeterministic workflow of role-scoped subagents (`planner → worker → verifier`);\nyou are the orchestrator that scopes the request and kicks the workflow.\n\n## To run\n\n1. Gather the request, then run the workflow as a tool call (so the loop-level\n   hook gates fire), passing structured input via `args`:\n\n   ```\n   WorkflowRun({{ \"script_path\": \".thclaws/state/workflows/run.js\",\n                 \"args\": {{ \"request\": \"<what the user wants>\" }} }})\n   ```\n\n2. Read the workflow's JSON summary and brief the user. If `ok` is false, say\n   which stage failed plainly.\n\n## The team (the workflow calls these via `thclaws.subagent({{agent: …}})`)\n\n| Subagent | Tools | Writes? | Role |\n|---|---|:--:|---|\n| `planner` | Read, Grep, Glob | ❌ | request → ordered steps |\n| `worker` | Read, Write, Edit, Bash | ✅ (output/**) | execute one step |\n| `verifier` | Read, Bash, Grep | ❌ | independent pass/fail gate |\n\n## Make it yours\n\nReplace the subagent prompts with your real roles, wire the actual work into\n`worker` (and the workflow), and run `thclaws agent validate .` before publishing.\n"
        )
    } else {
        format!(
            "# {slug}\n\nScaffolded with `thclaws agent new --pattern dynamic`. This agent reasons about\neach request and orchestrates role-scoped subagents via the `Task` tool — no\nfixed workflow, because the steps depend on the input.\n\n## Loop\n\n1. Understand the request (ask one short clarifying question only if genuinely\n   ambiguous).\n2. `Task(agent: \"planner\")` to break it into steps.\n3. For each step, `Task(agent: \"worker\")`.\n4. `Task(agent: \"verifier\")` to confirm the result on disk before you call it\n   done — never trust a worker's self-report.\n5. Brief the user; name 1–3 follow-ups.\n\n## The team\n\n| Subagent | Tools | Writes? | Role |\n|---|---|:--:|---|\n| `planner` | Read, Grep, Glob | ❌ | request → ordered steps |\n| `worker` | Read, Write, Edit, Bash | ✅ (output/**) | execute one step |\n| `verifier` | Read, Bash, Grep | ❌ | independent pass/fail gate |\n\n## Make it yours\n\nReplace the subagent prompts with your real roles and run\n`thclaws agent validate .` before publishing.\n"
        )
    };
    write(&mut files, dir, "AGENTS.md", &agents_md)?;

    // ── workflow (static / batch-fanout only) ──────────────────────────────
    if static_like {
        let work_phase = if pattern == "batch-fanout" {
            // 48.1/50: thclaws.parallel runs the workers CONCURRENTLY (plain
            // Promise.all over thclaws.subagent would be serial). It SETTLES —
            // a failed worker becomes its `fallback` instead of aborting the
            // batch — and the read-only verifier gate below catches any gaps.
            "    thclaws.log(`working ${plan.steps.length} step(s) concurrently`);\n    thclaws.parallel(plan.steps.map((s) => ({ agent: \"worker\",\n      prompt: \"Do this step: \" + JSON.stringify(s),\n      budget: { time: \"300s\" }, fallback: { status: \"failed\", step: s } })));"
        } else {
            "    for (const s of plan.steps) {\n      thclaws.log(`working: ${s.title}`);\n      await step({ agent: \"worker\", prompt: \"Do this step: \" + JSON.stringify(s),\n                   budget: { tokens: 40000, time: \"300s\" }, retry: 1 }, null);\n    }"
        };
        let wf = format!(
            "// Scaffolded by `thclaws agent new --pattern {pattern}`.\n// Deterministic pipeline: planner -> worker(s) -> verifier gate. Reads structured\n// input from `args` (WorkflowRun({{script_path, args}})). \"Done\" is decided by the\n// read-only verifier, never by a worker's self-report.\n\nasync function step(opts, fallback) {{\n  try {{ return await thclaws.subagent(opts); }}\n  catch (e) {{ thclaws.log(`step ${{opts.agent}} failed: ${{e}}`); return fallback; }}\n}}\n\nconst input = args || {{}};\nthclaws.log(`start: ${{JSON.stringify(input)}}`);\n\nconst plan = await step({{ agent: \"planner\",\n  prompt: \"Plan the work for this request. Return ONLY the JSON your role specifies.\\n\\nREQUEST: \" + JSON.stringify(input),\n  budget: {{ tokens: 20000, time: \"180s\" }}, retry: 2 }}, null);\n\nif (!plan || !Array.isArray(plan.steps) || plan.steps.length === 0) {{\n  globalThis.__wf_result = JSON.stringify({{ ok: false, stage: \"plan\", error: \"planner returned no steps\" }});\n}} else {{\n{work_phase}\n\n  // Independent verifier gate (bounded loop).\n  let gate = {{ ok: false }};\n  for (let i = 0; i < 2; i++) {{\n    gate = await step({{ agent: \"verifier\",\n      prompt: \"Verify the result on disk and return ONLY JSON {{ok, errors}}.\",\n      budget: {{ tokens: 20000, time: \"150s\" }}, retry: 2 }},\n      {{ ok: false, errors: [\"verifier returned nothing\"] }});\n    thclaws.log(`verify round ${{i + 1}}: ok=${{gate.ok}}`);\n    if (gate.ok) break;\n    await step({{ agent: \"worker\",\n      prompt: \"Fix exactly these issues, nothing else:\\n\" + JSON.stringify(gate.errors || []),\n      budget: {{ tokens: 40000, time: \"300s\" }}, retry: 1 }}, null);\n  }}\n\n  globalThis.__wf_result = JSON.stringify({{ ok: !!gate.ok, steps: plan.steps.length }});\n}}\n"
        );
        write(&mut files, dir, ".thclaws/agent_workflow/run.js", &wf)?;
    }

    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_pattern_scaffolds_a_valid_agent() {
        for pat in PATTERNS {
            let d = tempfile::tempdir().unwrap();
            let dir = d.path().join("demo-agent");
            let files = scaffold_agent(&dir, pat, None, false).unwrap();
            assert!(
                files.contains(&"AGENTS.md".to_string()),
                "{pat}: no AGENTS.md"
            );
            assert_eq!(
                *pat != "dynamic",
                files.iter().any(|f| f.ends_with("run.js")),
                "{pat}: workflow presence wrong"
            );
            let report = crate::cloud::agent_cli::validate_folder(&dir);
            assert!(
                report.ok(),
                "{pat}: scaffold must validate, errors: {:?}",
                report.errors
            );
        }
    }

    #[test]
    fn refuses_unknown_pattern_and_nonempty_dir() {
        let d = tempfile::tempdir().unwrap();
        assert!(scaffold_agent(d.path(), "bogus", None, false).is_err());
        // non-empty dir without --force
        std::fs::write(d.path().join("x.txt"), "x").unwrap();
        assert!(scaffold_agent(d.path(), "dynamic", None, false).is_err());
        assert!(scaffold_agent(d.path(), "dynamic", None, true).is_ok());
    }
}
