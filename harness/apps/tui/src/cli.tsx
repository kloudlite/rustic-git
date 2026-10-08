#!/usr/bin/env bun
// The pod's TUI: the agent in this process.
import { LocalBackend } from "@kloudlite-tui/backend/local";
import { boot } from "./hello.ts";
import { start } from "./start.tsx";

const local = new LocalBackend();
boot(local, await local.hello());
await start();
