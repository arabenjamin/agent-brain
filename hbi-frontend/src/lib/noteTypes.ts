import type { Tone } from "../components/ui";
import type { NoteType } from "./lists.generated";

/** One colour per note type, everywhere a note is shown; every type is named. */
export const NOTE_TYPE_TONE: Record<string, Tone> = {
  semantic: "accent",
  episodic: "cyan",
  reflection: "purple",
  consolidated: "green",
  outcome: "yellow",
  inference: "red",
  claim: "orange",
  source_record: "muted",
  news: "muted",
  news_raw: "muted",
  unsourced_synthesis: "yellow",
  meta_learning_result: "muted",
} satisfies Record<NoteType, Tone>;

export function noteTone(type: string | null | undefined): Tone {
  return (type && NOTE_TYPE_TONE[type]) || "muted";
}
