#!/usr/bin/env python3
"""A fake local LLM server for the install e2e (card #296). Standard library only.

    fake_engine.py --flavor sglang|vllm|ollama|llamacpp --port 30000 [--host 127.0.0.1]

It answers the handful of GETs lss uses to FIND and READ an engine (see crates/lss-core/src/engine.rs:
each adapter's detect()/identity()/scrape()) with the same shapes the real engines publish, and
nothing else - it generates no tokens. Every flavor serves a model whose id names the flavor
(`e2e-fake-<flavor>`), so an assertion can prove lss is reading THIS server and not some other
engine that happens to be listening on the machine. The token counters grow with wall-clock time,
so the collector sees a live, working engine rather than a frozen one.

All metric values are synthetic (never copied from a real deployment).
"""
import argparse
import json
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

START = time.time()


def grown(base, per_sec):
    return int(base + per_sec * (time.time() - START))


def sglang_metrics(model):
    lab = f'model_name="{model}",priority="",tp_rank="0"'
    gen = grown(100000, 40)
    prompt = grown(400000, 120)
    reqs = grown(500, 0.2)
    lines = [
        "# HELP sglang:num_running_reqs The number of running requests.",
        "# TYPE sglang:num_running_reqs gauge",
        f"sglang:num_running_reqs{{{lab}}} 1.0",
        "# TYPE sglang:num_queue_reqs gauge",
        f"sglang:num_queue_reqs{{{lab}}} 0.0",
        "# TYPE sglang:gen_throughput gauge",
        f"sglang:gen_throughput{{{lab}}} 42.5",
        "# TYPE sglang:token_usage gauge",
        f"sglang:token_usage{{{lab}}} 0.12",
        "# TYPE sglang:kv_used_tokens gauge",
        f"sglang:kv_used_tokens{{{lab}}} 12000.0",
        "# TYPE sglang:max_total_num_tokens gauge",
        f"sglang:max_total_num_tokens{{{lab}}} 100000.0",
        "# TYPE sglang:cache_hit_rate gauge",
        f"sglang:cache_hit_rate{{{lab}}} 0.5",
        "# TYPE sglang:context_len gauge",
        f"sglang:context_len{{{lab}}} 32768.0",
        "# TYPE sglang:prompt_tokens_total counter",
        f"sglang:prompt_tokens_total{{{lab}}} {prompt}.0",
        "# TYPE sglang:generation_tokens_total counter",
        f"sglang:generation_tokens_total{{{lab}}} {gen}.0",
        "# TYPE sglang:num_requests_total counter",
        f"sglang:num_requests_total{{{lab}}} {reqs}.0",
        "# TYPE sglang:cached_tokens_total counter",
        f"sglang:cached_tokens_total{{{lab}}} {prompt // 2}.0",
    ]
    return "\n".join(lines) + "\n"


def vllm_metrics(model):
    lab = f'engine="0",model_name="{model}"'
    lines = [
        "# TYPE vllm:num_requests_running gauge",
        f"vllm:num_requests_running{{{lab}}} 1.0",
        "# TYPE vllm:num_requests_waiting gauge",
        f"vllm:num_requests_waiting{{{lab}}} 0.0",
        "# TYPE vllm:kv_cache_usage_perc gauge",
        f"vllm:kv_cache_usage_perc{{{lab}}} 0.12",
        "# TYPE vllm:prompt_tokens_total counter",
        f"vllm:prompt_tokens_total{{{lab}}} {grown(400000, 120)}.0",
        "# TYPE vllm:generation_tokens_total counter",
        f"vllm:generation_tokens_total{{{lab}}} {grown(100000, 40)}.0",
    ]
    return "\n".join(lines) + "\n"


def llamacpp_metrics():
    lines = [
        "# TYPE llamacpp:prompt_tokens_total counter",
        f"llamacpp:prompt_tokens_total {grown(40000, 50)}",
        "# TYPE llamacpp:tokens_predicted_total counter",
        f"llamacpp:tokens_predicted_total {grown(20000, 20)}",
        "# TYPE llamacpp:requests_processing gauge",
        "llamacpp:requests_processing 1",
        "# TYPE llamacpp:requests_deferred gauge",
        "llamacpp:requests_deferred 0",
        "# TYPE llamacpp:n_tokens_max counter",
        "llamacpp:n_tokens_max 4096",
    ]
    return "\n".join(lines) + "\n"


class Engine(BaseHTTPRequestHandler):
    flavor = "sglang"
    model = ""
    server_version = "fake-engine/1"

    def log_message(self, fmt, *args):  # quiet: the e2e prints its own transcript
        pass

    def send(self, code, body, ctype="application/json"):
        data = body.encode() if isinstance(body, str) else body
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def js(self, obj):
        self.send(200, json.dumps(obj))

    def do_GET(self):
        f = self.flavor
        model = self.model or f"e2e-fake-{f}"
        p = self.path.split("?")[0]
        owned = {"sglang": "sglang", "vllm": "vllm", "ollama": "library", "llamacpp": "llamacpp"}[f]
        models = {"object": "list", "data": [{"id": model, "object": "model", "created": int(START), "owned_by": owned, "max_model_len": 32768}]}
        if p in ("/health", "/healthz"):
            return self.send(200, "{}")
        if p == "/v1/models":
            return self.js(models)
        if f == "sglang":
            if p == "/metrics":
                return self.send(200, sglang_metrics(model), "text/plain; version=0.0.4")
            if p == "/get_server_info":
                return self.js({"version": "0.0.0-e2e", "max_running_requests": 8, "context_length": 32768, "max_total_num_tokens": 100000, "model_path": model})
            if p == "/get_model_info":
                return self.js({"model_path": model})
        if f == "vllm":
            if p == "/metrics":
                return self.send(200, vllm_metrics(model), "text/plain; version=0.0.4")
            if p == "/version":
                return self.js({"version": "0.0.0-e2e"})
        if f == "llamacpp":
            if p == "/metrics":
                return self.send(200, llamacpp_metrics(), "text/plain; version=0.0.4")
            if p == "/props":
                return self.js({"model_alias": model, "model_path": f"/models/{model}.gguf", "build_info": "b0-e2e", "total_slots": 1, "default_generation_settings": {"n_ctx": 4096}})
            if p == "/slots":
                return self.js([{"id": 0, "is_processing": False, "n_ctx": 4096}])
        if f == "ollama":
            if p == "/api/version":
                return self.js({"version": "0.0.0-e2e"})
            if p == "/api/tags":
                return self.js({"models": [{"name": model, "model": model, "size": 1, "details": {"parameter_size": "1B", "quantization_level": "Q4_0"}}]})
            if p == "/api/ps":
                return self.js({"models": [{"name": model, "model": model, "size": 1, "size_vram": 1, "context_length": 4096, "details": {"parameter_size": "1B", "quantization_level": "Q4_0"}}]})
            if p == "/":
                return self.send(200, "Ollama is running", "text/plain")
        return self.send(404, '{"error":"not found"}')


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--flavor", default="sglang", choices=["sglang", "vllm", "ollama", "llamacpp"])
    ap.add_argument("--port", type=int, default=30000)
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--model", default="", help="the served model id (default e2e-fake-<flavor>)")
    a = ap.parse_args()
    Engine.flavor = a.flavor
    Engine.model = a.model
    srv = ThreadingHTTPServer((a.host, a.port), Engine)
    print(f"fake engine {a.flavor} on http://{a.host}:{a.port} (model {a.model or 'e2e-fake-' + a.flavor})", flush=True)
    srv.serve_forever()


if __name__ == "__main__":
    main()
