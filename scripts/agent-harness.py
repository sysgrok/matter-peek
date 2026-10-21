#!/usr/bin/env python3
"""A throwaway agent loop: a model behind an OpenAI-compatible API (e.g. Ollama)
drives matter-peek's MCP tools.

It starts `matter-peek mcp` (newline-delimited JSON-RPC over stdio), offers its
tools to the model as OpenAI function tools, runs the model's tool calls
through MCP and feeds the results back, until the model answers. Every tool
call and result is printed; the ones that change something are asked for
first.

Examples:
    scripts/agent-harness.py "Which of my devices are offline?"
    scripts/agent-harness.py --allow-mutate                 # chat, may change devices
    scripts/agent-harness.py --server ws://127.0.0.1:8087   # use a running server
    scripts/agent-harness.py --llm-url https://api.openai.com/v1 --model gpt-5 --api-key-env OPENAI_API_KEY

In the chat: /reset forgets the conversation, /tools lists the tools, /quit ends.
"""

import argparse
import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

try:
    from rich.console import Console
    from rich.markdown import Markdown
except ImportError:
    Console = None

REPO = Path(__file__).resolve().parent.parent

DEFAULT_LLM_URL = "http://192.168.10.189:11434/v1"
DEFAULT_MODEL = "qwen3.5:9b-q4_K_M"

SYSTEM_PROMPT = """\
You are the assistant of matter-peek, a local Matter smart-home controller. \
You inspect, diagnose and repair the user's Matter devices only through the \
tools you are given.

- Look things up with the tools; never guess device state.
- Use node ids exactly as the tools return them; never convert numbers between \
hex and decimal yourself.
- Before a tool that changes something, say in one sentence what it will do.
- Answer briefly and plainly: what you found, and what to do next.
- When a tool fails, tell the user in plain words what failed and why (its \
error says), and what they can do about it.
- Write your answers in Markdown for a human: short paragraphs, **bold** for \
the key facts, a table when you show several devices or values. Name devices \
by their product name, not their node id. Never paste raw JSON."""

# Tool output kept for the model, and shown to the user.
MODEL_RESULT_MAX = 12_000
SHOWN_RESULT_MAX = 800
MAX_STEPS = 16


class Style:
    on = sys.stdout.isatty()

    @classmethod
    def wrap(cls, code, text):
        return f"\033[{code}m{text}\033[0m" if cls.on else text

    dim = classmethod(lambda cls, t: cls.wrap("2", t))
    tool = classmethod(lambda cls, t: cls.wrap("36", t))
    warn = classmethod(lambda cls, t: cls.wrap("33", t))
    error = classmethod(lambda cls, t: cls.wrap("31", t))
    bold = classmethod(lambda cls, t: cls.wrap("1", t))


def show_markdown(text):
    """Print `text` rendered as Markdown where the terminal allows it."""
    if Console is not None and sys.stdout.isatty():
        Console().print(Markdown(text))
    else:
        print(text)


def error_text(text):
    """The message of a failed tool's result, without its JSON wrapping."""
    body = text.removeprefix("ERROR: ")
    try:
        parsed = json.loads(body)
    except json.JSONDecodeError:
        return body
    return parsed.get("error", body) if isinstance(parsed, dict) else body


class Mcp:
    """A `matter-peek mcp` child process, spoken to over stdio."""

    def __init__(self, command, log_path):
        self.log = open(log_path, "a", encoding="utf-8")
        self.proc = subprocess.Popen(
            command,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=self.log,
            text=True,
            bufsize=1,
        )
        self.next_id = 1

    def request(self, method, params=None):
        msg = {"jsonrpc": "2.0", "id": self.next_id, "method": method}
        if params is not None:
            msg["params"] = params
        self.next_id += 1
        self._send(msg)

        while True:
            line = self.proc.stdout.readline()
            if not line:
                raise RuntimeError(f"the MCP server exited: {self.last_log_line()}")
            reply = json.loads(line)
            if reply.get("id") == msg["id"]:
                break

        if "error" in reply:
            raise RuntimeError(f"{method}: {reply['error'].get('message')}")
        return reply["result"]

    def last_log_line(self):
        self.log.flush()
        try:
            lines = Path(self.log.name).read_text(errors="replace").strip().splitlines()
        except OSError:
            lines = []
        return lines[-1] if lines else f"see {self.log.name}"

    def notify(self, method):
        self._send({"jsonrpc": "2.0", "method": method})

    def _send(self, msg):
        self.proc.stdin.write(json.dumps(msg) + "\n")
        self.proc.stdin.flush()

    def close(self):
        if self.proc.poll() is None:
            self.proc.stdin.close()
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.kill()
        self.log.close()


class Llm:
    """An OpenAI-compatible Chat Completions endpoint."""

    def __init__(self, url, model, api_key, think):
        self.url = url.rstrip("/") + "/chat/completions"
        self.model = model
        self.api_key = api_key
        self.think = think

    def complete(self, messages, tools):
        body = {"model": self.model, "messages": messages, "tools": tools}
        if not self.think:
            body["reasoning_effort"] = "none"

        req = urllib.request.Request(
            self.url,
            data=json.dumps(body).encode(),
            headers={
                "Content-Type": "application/json",
                "Authorization": f"Bearer {self.api_key}",
            },
        )
        try:
            with urllib.request.urlopen(req, timeout=600) as resp:
                reply = json.load(resp)
        except urllib.error.HTTPError as e:
            raise RuntimeError(f"LLM HTTP {e.code}: {e.read().decode(errors='replace')[:500]}")

        return reply["choices"][0]["message"], reply.get("usage") or {}


def openai_tools(mcp_tools):
    return [
        {
            "type": "function",
            "function": {
                "name": t["name"],
                "description": t.get("description", ""),
                "parameters": t.get("inputSchema") or {"type": "object", "properties": {}},
            },
        }
        for t in mcp_tools
    ]


def result_text(result):
    text = "\n".join(
        c.get("text", "") for c in result.get("content", []) if c.get("type") == "text"
    )
    return ("ERROR: " if result.get("isError") else "") + text


def clip(text, limit):
    if len(text) <= limit:
        return text
    return text[:limit] + f"\n… ({len(text) - limit} more characters cut)"


class Agent:
    def __init__(self, args, mcp, llm, mcp_tools, instructions):
        self.args = args
        self.instructions = instructions
        self.mcp = mcp
        self.llm = llm
        self.mcp_tools = {t["name"]: t for t in mcp_tools}
        self.tools = openai_tools(mcp_tools)
        self.always = set()
        self.reset()

    def reset(self):
        system = self.args.system or SYSTEM_PROMPT
        if self.instructions:
            system += "\n\nAbout the tools:\n" + self.instructions
        self.messages = [{"role": "system", "content": system}]

    def changes_things(self, name):
        """Whether a tool may change something: unless the server marks it
        read-only."""
        annotations = self.mcp_tools.get(name, {}).get("annotations") or {}
        return annotations.get("readOnlyHint") is not True

    def approve(self, name, arguments):
        if self.args.yes or name in self.always:
            return True
        if not (self.args.confirm_all or self.changes_things(name)):
            return True

        while True:
            answer = input(Style.warn(f"  Run {name}? [y]es / [n]o / [a]lways: ")).strip().lower()
            if answer in ("y", "yes"):
                return True
            if answer in ("a", "always"):
                self.always.add(name)
                return True
            if answer in ("n", "no", ""):
                return False

    def turn(self, user_text):
        self.messages.append({"role": "user", "content": user_text})

        for _ in range(MAX_STEPS):
            started = time.monotonic()
            message, usage = self.llm.complete(self.messages, self.tools)
            took = time.monotonic() - started

            if self.args.show_thinking and message.get("reasoning"):
                print(Style.dim(message["reasoning"].strip()))
            if self.args.verbose:
                print(Style.dim(
                    f"  [model: {took:.1f} s, {usage.get('prompt_tokens', '?')} in / "
                    f"{usage.get('completion_tokens', '?')} out]"
                ))

            calls = message.get("tool_calls") or []
            assistant = {"role": "assistant", "content": message.get("content") or ""}
            if calls:
                assistant["tool_calls"] = calls
            self.messages.append(assistant)

            if message.get("content"):
                show_markdown(message["content"].strip())
            if not calls:
                if not message.get("content"):
                    self.ask_for_answer()
                return

            for call in calls:
                self.messages.append({
                    "role": "tool",
                    "tool_call_id": call.get("id", ""),
                    "content": self.run_tool(call),
                })

        print(Style.warn(f"  Stopped after {MAX_STEPS} steps without an answer."))

    def ask_for_answer(self):
        """Some models end with their answer in their thinking and an empty
        reply: ask once more, without thinking, for the answer itself."""
        self.messages[-1:] = []
        self.messages.append({
            "role": "user",
            "content": "Write your answer to me now, in Markdown, without calling more tools.",
        })
        think, self.llm.think = self.llm.think, False
        try:
            message, _ = self.llm.complete(self.messages, self.tools)
        finally:
            self.llm.think = think
        self.messages[-1:] = []

        if message.get("content"):
            self.messages.append({"role": "assistant", "content": message["content"]})
            show_markdown(message["content"].strip())
        else:
            print(Style.dim("  (The model ended without an answer.)"))

    def run_tool(self, call):
        name = call["function"]["name"]
        raw = call["function"].get("arguments") or "{}"
        shown_args = "" if raw in ("{}", {}) else f" {raw}"
        print(Style.tool(f"→ {name}{shown_args}"), end="", flush=True)

        try:
            arguments = json.loads(raw) if isinstance(raw, str) else raw
        except json.JSONDecodeError as e:
            text = f"ERROR: the arguments are not valid JSON: {e}"
            print(Style.error(f"  ✗ {text}"))
            return text

        if name not in self.mcp_tools:
            text = f"ERROR: there is no tool named {name}"
            print(Style.error(f"  ✗ {text}"))
            return text

        if self.changes_things(name) and not self.args.yes and name not in self.always:
            print()
        if not self.approve(name, arguments):
            print(Style.warn("  Refused."))
            return "The user refused to run this tool."

        started = time.monotonic()
        try:
            text = result_text(self.mcp.request("tools/call", {"name": name, "arguments": arguments}))
        except RuntimeError as e:
            text = f"ERROR: {e}"
        took = time.monotonic() - started

        if text.startswith("ERROR"):
            print(Style.error(f"  ✗ {error_text(text)}") + Style.dim(f"  ({took:.1f} s)"))
        else:
            print(Style.dim(f"  ✓ ({took:.1f} s)"))
        if self.args.verbose:
            shown = clip(text, SHOWN_RESULT_MAX)
            print(Style.dim("    " + shown.replace("\n", "\n    ")))

        return clip(text, MODEL_RESULT_MAX)


def main():
    p = argparse.ArgumentParser(
        description=__doc__.split("\n\n")[0],
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    p.add_argument("prompt", nargs="?", help="Ask this, answer, and exit (else chat).")
    p.add_argument("--llm-url", default=os.environ.get("LLM_URL", DEFAULT_LLM_URL))
    p.add_argument("--model", default=os.environ.get("LLM_MODEL", DEFAULT_MODEL))
    p.add_argument("--api-key-env", default="LLM_API_KEY",
                   help="Environment variable holding the API key (Ollama needs none).")
    p.add_argument("--no-think", action="store_true",
                   help="Ask the model not to think first (faster, less careful).")
    p.add_argument("--show-thinking", action="store_true", help="Print the model's thinking.")
    p.add_argument("--verbose", action="store_true",
                   help="Print the tools' raw results, and the model's timings and tokens.")
    p.add_argument("--matter-peek", default=str(REPO / "target/debug/matter-peek"),
                   help="The matter-peek binary.")
    p.add_argument("--server", help="Use a running matter-peek server (ws://…) instead of an "
                   "embedded controller.")
    p.add_argument("--allow-mutate", action="store_true",
                   help="Offer the tools that change devices (each asked for first).")
    p.add_argument("--confirm-all", action="store_true", help="Ask before every tool call.")
    p.add_argument("--yes", action="store_true", help="Never ask (careful).")
    p.add_argument("--system", help="Replace the system prompt.")
    p.add_argument("--mcp-log", default=str(REPO / "target/agent-harness-mcp.log"),
                   help="Where the MCP server's log goes.")
    args = p.parse_args()

    command = [args.matter_peek]
    if args.server:
        command += ["--server", args.server]
    command += ["mcp"]
    if args.allow_mutate:
        command += ["--allow-mutate"]

    mcp = Mcp(command, args.mcp_log)
    try:
        main_loop(args, mcp)
    except RuntimeError as e:
        print(Style.error(str(e)))
        sys.exit(1)
    finally:
        mcp.close()


def main_loop(args, mcp):
    init = mcp.request("initialize", {
        "protocolVersion": "2025-06-18",
        "capabilities": {},
        "clientInfo": {"name": "agent-harness", "version": "0"},
    })
    mcp.notify("notifications/initialized")
    mcp_tools = mcp.request("tools/list")["tools"]

    llm = Llm(args.llm_url, args.model, os.environ.get(args.api_key_env, "none"),
              think=not args.no_think)
    agent = Agent(args, mcp, llm, mcp_tools, init.get("instructions", ""))
    print(Style.dim(f"{args.model} at {args.llm_url}; {len(mcp_tools)} tools; "
                    f"MCP log: {args.mcp_log}"))

    if args.prompt:
        agent.turn(args.prompt)
        return

    while True:
        try:
            text = input(Style.bold("\nyou> ")).strip()
        except (EOFError, KeyboardInterrupt):
            print()
            return
        if not text:
            continue
        if text in ("/quit", "/exit"):
            return
        if text == "/reset":
            agent.reset()
            print(Style.dim("  Forgotten."))
            continue
        if text == "/tools":
            for name, tool in agent.mcp_tools.items():
                mark = " (changes things)" if agent.changes_things(name) else ""
                print(f"  {name}{mark}")
            continue
        try:
            agent.turn(text)
        except (RuntimeError, urllib.error.URLError) as e:
            print(Style.error(f"  {e}"))
        except KeyboardInterrupt:
            print(Style.warn("  Interrupted."))


if __name__ == "__main__":
    main()
