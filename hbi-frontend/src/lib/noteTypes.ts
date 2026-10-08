import type { Tone } from "../components/ui";

/** One colour per note type, everywhere a note is shown. */
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
  unsourced_synthesis: "yellow",
};

export function noteTone(type: string | null | undefined): Tone {
  return (type && NOTE_TYPE_TONE[type]) || "muted";
}
