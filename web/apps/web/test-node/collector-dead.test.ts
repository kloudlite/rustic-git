import { test } from "node:test";
import { requestsSurvive } from "./collector.ts";

test("a dead collector never fails or stalls a request", () => requestsSurvive("http://127.0.0.1:9"));
