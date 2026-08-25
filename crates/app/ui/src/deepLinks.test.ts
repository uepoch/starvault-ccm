import { describe, expect, it } from "vite-plus/test";
import { parseTranslatorInstallUrl } from "./deepLinks";

describe("translator install deep links", () => {
  it("accepts the exact translator install route", () => {
    expect(parseTranslatorInstallUrl("starvault://install/translator/upload-wpRtPJWdAa")).toBe(
      "upload-wpRtPJWdAa",
    );
  });

  it.each([
    "https://install/translator/upload-wpRtPJWdAa",
    "starvault://other/translator/upload-wpRtPJWdAa",
    "starvault://install/download/upload-wpRtPJWdAa",
    "starvault://user@install/translator/upload-wpRtPJWdAa",
    "starvault://install:42/translator/upload-wpRtPJWdAa",
    "starvault://install/translator/upload-wpRtPJWdAa?source=web",
    "starvault://install/translator/upload-wpRtPJWdAa#fragment",
    "starvault://install/translator/upload-wpRtPJWdAa/extra",
    "starvault://install/translator/upload-%2e%2e",
    "starvault://install/translator/upload-%2Fescape",
    "starvault://install/translator/",
    "starvault://install/translator/upload-with.dot",
    `starvault://install/translator/upload-${"a".repeat(65)}`,
  ])("rejects malformed or unrelated route %s", (value) => {
    expect(parseTranslatorInstallUrl(value)).toBeNull();
  });
});
