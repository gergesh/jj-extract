#!/usr/bin/env python3
"""Five concurrent hook clients, seeded edit schedules, and real rustfmt sweeps.

Run directly to reproduce a seed, or through scripts/check.sh. Formatters run
between batches of file tools, while all five agent sessions are mid-task. File
tools within each batch contend on the actual jj-extract edit lock; the test
does not supply a replacement lock. No extraction uses --allow-conflicts.
"""

import argparse
from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import random
import re
import subprocess
import tempfile


ROOT = Path(__file__).resolve().parents[1]
FILES = ("shared.rs", "settings.rs")
FORMATTED = (*FILES, "handoff.rs")
AGENTS = 5
ROUNDS = 4


class Scenario:
    def __init__(self, directory, binary, seed):
        self.root = Path(directory)
        self.binary = binary
        self.seed = seed
        self.random = random.Random(seed)
        self.env = dict(os.environ, JJ_GIT_OK="1", JJ_EDITOR="true",
                        JJ_EXTRACT_HOME=str(self.root / ".hook-state"))
        for name in ("JJ_EXTRACT_AGENT", "CLAUDE_CODE_SESSION_ID", "CODEX_THREAD_ID"):
            self.env.pop(name, None)
        self.ids = {}
        self.expected = [0] * AGENTS
        self.formats = 0
        self.handoffs = False
        self.history = []

    def run(self, *args, input=None):
        result = subprocess.run(args, cwd=self.root, env=self.env, input=input,
                                text=True, capture_output=True, timeout=90)
        if result.returncode:
            raise AssertionError(f"seed {self.seed}: {args}\n{result.stdout}{result.stderr}")
        return result.stdout

    def session(self, agent):
        return f"parallel-{self.seed}-{agent}"

    def hook(self, event, agent, paths=FILES):
        payload = {
            "hook_event_name": event,
            "cwd": str(self.root),
            "session_id": self.session(agent),
            "tool_name": "apply_patch",
            "tool_input": {"command": "*** Begin Patch\n"
                           + "".join(f"*** Update File: {path}\n" for path in paths)
                           + "*** End Patch"},
        }
        self.run(self.binary, "--hook", input=json.dumps(payload))

    def setup(self):
        self.run("jj", "git", "init", ".")
        self.run("jj", "config", "set", "--repo", "user.name", "tester")
        self.run("jj", "config", "set", "--repo", "user.email", "test@example.com")
        self.run("jj", "config", "set", "--repo", "snapshot.auto-track", "none()")
        functions = "\n".join(f"pub fn task_{i}()->usize{{0}}" for i in range(AGENTS))
        (self.root / FILES[0]).write_text(functions + '\npub const MESSAGE: &str = "keep  two spaces";\n')
        fields = ",".join(f"slot_{i}:usize" for i in range(AGENTS))
        values = ",".join(f"slot_{i}:0" for i in range(AGENTS))
        (self.root / FILES[1]).write_text(
            f"pub struct Config{{{fields}}}\npub fn config()->Config{{Config{{{values}}}}}\n")
        (self.root / "neutral.txt").write_text("base\n")
        (self.root / "handoff.rs").write_text("pub fn handoff()->usize{0}\n")
        self.run("jj", "file", "track", *FORMATTED, "neutral.txt")
        self.run("jj", "commit", "-m", "base")

    def edit(self, agent, value):
        # Read only after PreToolUse has acquired the real cross-process lock.
        self.hook("PreToolUse", agent)
        try:
            self.history.append(f"agent {agent}: {value}")
            path = self.root / FILES[0]
            text, count = re.subn(rf"pub fn task_{agent}\(\)\s*->\s*usize\s*\{{\s*\d+\s*\}}",
                                  f"pub fn task_{agent}()->usize{{{value}}}", path.read_text())
            assert count == 1
            path.write_text(text)
            path = self.root / FILES[1]
            text, count = re.subn(rf"slot_{agent}:\s*\d+", f"slot_{agent}:{value}", path.read_text())
            assert count == 1
            path.write_text(text)
        finally:
            self.hook("PostToolUse", agent)

    def format(self):
        width = self.random.choice((40, 80, 120))
        self.history.append(f"rustfmt: max_width={width}")
        self.run("rustfmt", "--edition", "2021", "--config", f"max_width={width}", *FORMATTED)
        self.formats += 1

    def values(self, revision):
        shared = self.run("jj", "file", "show", "-r", revision, FILES[0])
        settings = self.run("jj", "file", "show", "-r", revision, FILES[1])
        assert '"keep  two spaces"' in shared, "string whitespace changed"
        functions = dict((int(i), int(value)) for i, value in re.findall(
            r"pub fn task_(\d+)\(\)\s*->\s*usize\s*\{\s*(\d+)\s*\}", shared))
        fields = dict((int(i), int(value)) for i, value in re.findall(r"slot_(\d+):\s*(\d+)", settings))
        assert set(functions) == set(fields) == set(range(AGENTS))
        return functions, fields

    def extract(self, agent=None, preview=False):
        before = self.run("jj", "log", "-r", "@", "--no-graph", "-T", "commit_id")
        operation = self.run("jj", "--ignore-working-copy", "op", "log", "--no-graph", "-n", "1",
                             "-T", "self.id()")
        disk = {path: (self.root / path).read_bytes() for path in (*FORMATTED, "neutral.txt")}
        args = [self.binary, "--all"] if agent is None else [self.binary, "--agent", self.session(agent)]
        if preview:
            args.append("--dry-run")
        output = self.run(*args)
        assert "CONFLICT" not in output, output
        for path, content in disk.items():
            assert (self.root / path).read_bytes() == content, f"live file changed: {path}"
        assert not self.run("jj", "diff", "--from", before, "--to", "@", "--summary").strip()
        if preview:
            assert self.run("jj", "log", "-r", "@", "--no-graph", "-T", "commit_id") == before
            assert self.run("jj", "--ignore-working-copy", "op", "log", "--no-graph", "-n", "1",
                            "-T", "self.id()") == operation
            return
        assert self.run("jj", "--ignore-working-copy", "op", "log", "--no-graph", "-n", "1",
                        "-T", 'self.parents().map(|p| p.id()).join("\\n")') == operation
        for index, change in re.findall(rf"session parallel-{self.seed}-(\d+) → change (\w+)", output):
            index = int(index)
            assert self.ids.get(index, change) == change, "re-extraction changed an ID"
            self.ids[index] = change
        for index, change in self.ids.items():
            before_values = self.values(change + "-")
            after_values = self.values(change)
            for before_file, after_file in zip(before_values, after_values):
                assert after_file[index] == self.expected[index], (index, after_file, self.expected)
                for other in range(AGENTS):
                    if other != index:
                        assert before_file[other] == after_file[other], f"agent {index} owns agent {other}'s edit"
            assert self.run("jj", "file", "show", "-r", change, "neutral.txt") == "base\n"
            handoff = self.run("jj", "file", "show", "-r", change, "handoff.rs")
            expected_handoff = (AGENTS - index) * 100 if self.handoffs else 0
            assert int(re.search(r"\{\s*(\d+)\s*\}", handoff)[1]) == expected_handoff
            if self.handoffs and index < AGENTS - 1:
                parent = self.run("jj", "log", "-r", change + "-", "--no-graph", "-T", "change_id.short()")
                assert parent == self.ids[index + 1], "handoff prerequisite is in the wrong position"
        assert self.run("jj", "log", "-r", "heads(all()) ~ root()", "--no-graph", "-T", '"head\\n"') == "head\n"
        assert not self.run("jj", "log", "-r", "all()", "--no-graph",
                            "-T", 'if(conflict || divergent, "bad", "")').strip()

    def exercise(self):
        self.setup()
        counts = [0] * AGENTS
        mid_extracted = False
        with ThreadPoolExecutor(max_workers=AGENTS) as workers:
            while sum(counts) < AGENTS * ROUNDS:
                available = [agent for agent in range(AGENTS) if counts[agent] < ROUNDS]
                # Start all five together; thereafter, interleave formatter runs
                # with irregular batches so no session has a fixed phase boundary.
                size = AGENTS if not sum(counts) else self.random.randint(1, len(available))
                schedule = self.random.sample(available, size)
                self.random.shuffle(schedule)
                jobs = []
                for agent in schedule:
                    counts[agent] += 1
                    self.expected[agent] = counts[agent] * 100 + agent
                    jobs.append(workers.submit(self.edit, agent, self.expected[agent]))
                for job in jobs:
                    job.result()
                if self.random.random() < 0.75 or sum(counts) == AGENTS * ROUNDS:
                    self.format()
                (self.root / "neutral.txt").write_text(f"base\nHUMAN-{self.seed}\n")
                # Continue editing after formatting, then extract mid-task so
                # later edits also exercise updating previously extracted IDs.
                if (sum(counts) >= 10 and not mid_extracted) or sum(counts) == AGENTS * ROUNDS:
                    self.extract(preview=True)
                    self.extract()
                    mid_extracted = True
        # A reverse handoff makes all five existing changes depend on a new
        # placement, while unrelated original contributions stay with their owner.
        for agent in reversed(range(AGENTS)):
            self.hook("PreToolUse", agent, ("handoff.rs",))
            self.history.append(f"handoff: agent {agent}")
            (self.root / "handoff.rs").write_text(
                f"pub fn handoff()->usize{{{(AGENTS - agent) * 100}}}\n")
            self.hook("PostToolUse", agent, ("handoff.rs",))
            self.format()
        self.handoffs = True
        self.extract(preview=True)
        self.extract()
        # All five sessions can subsequently extract independently in any order.
        schedule = list(range(AGENTS))
        self.random.shuffle(schedule)
        for agent in schedule:
            self.extract(agent)
        self.extract()
        assert len(self.ids) == AGENTS
        assert self.values("@")[0] == dict(enumerate(self.expected))
        assert "neutral.txt" in self.run("jj", "diff", "-r", "@", "--summary")
        print(f"PASS: seed {self.seed}, five concurrent agents, {AGENTS * (ROUNDS + 1)} edits, "
              f"{self.formats} rustfmt sweeps; clean attribution, IDs and live tree", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/jj-extract")
    parser.add_argument("--seed", type=int, action="append", help="reproduce a specific schedule")
    args = parser.parse_args()
    for seed in args.seed or (7, 42, 99):
        with tempfile.TemporaryDirectory(prefix=f"jj-extract-five-{seed}-") as directory:
            scenario = Scenario(directory, str(args.binary.resolve()), seed)
            try:
                scenario.exercise()
            except Exception:
                print(f"Failed seed {seed}; actual edit/format order:\n" + "\n".join(scenario.history), flush=True)
                raise


if __name__ == "__main__":
    main()
