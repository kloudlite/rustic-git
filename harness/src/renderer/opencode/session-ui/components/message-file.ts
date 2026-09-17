// harness: our own name table, so this file does not pull shiki's full grammar bundle in for a
// label (see ../../languages.ts).
import { LANGUAGE_NAMES } from "../../languages"
import { getFilename } from "@opencode-ai/core/util/path"
import type { FilePart } from "@opencode-ai/sdk/v2"

export function attached(part: FilePart) {
  return part.url.startsWith("data:") && !inline(part)
}

export function inline(part: FilePart) {
  return part.source?.text?.start !== undefined && part.source?.text?.end !== undefined
}

export function kind(part: FilePart) {
  return part.mime.startsWith("image/") ? "image" : "file"
}

// attachments carry text/plain for all text files, so the label comes from the extension;
// filename may be an absolute path, so extract the basename before looking for one
export function typeLabel(filename: string, mime: string, fallback: string) {
  if (mime === "application/pdf") return "PDF"
  const base = getFilename(filename)
  // idx 0 is a dotfile like .gitignore, not an extension
  const idx = base.lastIndexOf(".")
  const suffix = idx <= 0 ? "" : base.slice(idx + 1).toLowerCase()
  if (!suffix) return fallback
  return LANGUAGE_NAMES.get(suffix) ?? suffix.toUpperCase()
}
