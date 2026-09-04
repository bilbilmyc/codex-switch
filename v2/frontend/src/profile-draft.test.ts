import { describe, expect, it } from "vitest";
import { profileSchema, toProfileDraft } from "./profile-draft";

describe("profile draft", () => {
  it("keeps an empty API key absent so an existing secret is not overwritten", () => {
    const draft = toProfileDraft({
      name: "Relay A",
      baseUrl: "https://relay.example/v1",
      apiKey: " ",
      model: "gpt-5.2-codex",
      reviewModel: " ",
    });

    expect(draft.apiKey).toBeUndefined();
    expect(draft.reviewModel).toBeUndefined();
  });

  it("accepts only HTTP(S) relay endpoints", () => {
    const result = profileSchema.safeParse({
      name: "Relay A",
      baseUrl: "ftp://relay.example/v1",
      apiKey: "",
      model: "gpt-5.2-codex",
      reviewModel: "",
    });

    expect(result.success).toBe(false);
  });
});
