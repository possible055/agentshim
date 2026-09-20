// Event-driven waits for test code. Raw `setTimeout` sleeps cannot express
// "wait for the event" and turn slow machines into flaky tests; poll with a
// deadline instead. A deliberate sleep without an observable event (a race
// window, a negative-assertion window) stays inline with a `sleep-allow:`
// comment instead of living here.

export const POLL_INTERVAL_MS = 20

/**
 * Polls `condition` until it resolves to a truthy value, or throws naming
 * `what` once `deadlineMs` have elapsed.
 */
export async function waitForCondition(
  condition: () => boolean | Promise<boolean>,
  deadlineMs: number,
  what: string,
): Promise<void> {
  const deadline = Date.now() + deadlineMs
  for (;;) {
    if (await condition()) return
    if (Date.now() >= deadline) throw new Error(`timed out waiting for ${what}`)
    await new Promise(resolve => setTimeout(resolve, POLL_INTERVAL_MS))
  }
}

/**
 * Resolves after `ms`. Only for deliberate fixed windows; every
 * wait-for-an-event should use {@link waitForCondition} instead.
 */
export async function sleep(ms: number): Promise<void> {
  await new Promise(resolve => setTimeout(resolve, ms))
}
