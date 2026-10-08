/**
 * What the scheduler is doing, from `/api/scheduler/status`.
 *
 * `config.enabled` is the on/off switch (`scheduler_control start|stop` set
 * it; the error budget clears it). `state.is_running` only means a tick is
 * executing at this moment, so reading it as "on" reported a healthy,
 * sleeping scheduler as paused.
 */
import type { SchedulerStatus } from "../api/queries";
import type { Tone } from "../components/ui";

export type SchedulerPhase = "auto-paused" | "paused" | "ticking" | "sleeping" | "erroring" | "idle";

export interface SchedulerSummary {
  phase: SchedulerPhase;
  /** On/off as the user controls it. */
  enabled: boolean;
  label: string;
  tone: Tone;
  detail: string;
}

export function summariseScheduler(s: SchedulerStatus): SchedulerSummary {
  const { config, state } = s;
  const errors = state.consecutive_errors;
  if (!config.enabled) {
    const auto = errors >= config.error_budget && errors > 0;
    return auto
      ? {
          phase: "auto-paused",
          enabled: false,
          label: "auto-paused",
          tone: "red",
          detail: `Stopped itself after ${errors} consecutive errors${state.last_error ? `: ${state.last_error}` : ""}`,
        }
      : { phase: "paused", enabled: false, label: "paused", tone: "yellow", detail: "Turned off; no tasks are dispatched" };
  }
  if (state.is_running) {
    return { phase: "ticking", enabled: true, label: "ticking", tone: "green", detail: "A tick is dispatching tasks now" };
  }
  if (errors > 0) {
    return {
      phase: "erroring",
      enabled: true,
      label: `${errors}/${config.error_budget} errors`,
      tone: "red",
      detail: `Auto-pauses at ${config.error_budget} consecutive errors${state.last_error ? `. Last: ${state.last_error}` : ""}`,
    };
  }
  if (state.is_sleeping) {
    return {
      phase: "sleeping",
      enabled: true,
      label: "sleeping",
      tone: "cyan",
      detail: `Idle; ticking every ${Math.round(config.sleep_interval_secs / 60)} min until there is work`,
    };
  }
  return {
    phase: "idle",
    enabled: true,
    label: "on",
    tone: "green",
    detail: `Ticking every ${Math.round(config.interval_secs / 60)} min`,
  };
}
