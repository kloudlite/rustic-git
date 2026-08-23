"use client";

import { Check, Copy, Download } from "lucide-react";
import { Button } from "@/components/ui/button";
import { useCopy } from "@/lib/use-copy";

/** Take this file away: to the clipboard, or to disk.
 *
 *  Both are built from the text the page already has rather than from a second
 *  request — the bytes are here, and a "raw" round trip would only re-fetch what
 *  is on screen. A blob the api truncated saves what was served, which is why the
 *  header says so beside these buttons. */
export function FileActions({ text, filename }: { text: string; filename: string }) {
  const { copied, copy } = useCopy();

  return (
    <>
      <Button
        variant="ghost"
        size="sm"
        className="text-caption"
        onClick={() => copy(text)}
      >
        {copied ? <Check className="text-success" /> : <Copy />}
        {copied ? "Copied" : "Copy"}
      </Button>
      <Button
        variant="ghost"
        size="sm"
        className="text-caption"
        onClick={() => {
          const url = URL.createObjectURL(new Blob([text], { type: "text/plain" }));
          const a = document.createElement("a");
          a.href = url;
          a.download = filename;
          a.click();
          URL.revokeObjectURL(url);
        }}
      >
        <Download />
        Download
      </Button>
    </>
  );
}
