// The git review pane (spec: desktop-app, Git review pane): the session's
// branch, its distance from upstream and the changed files with their
// diffs in the `uncommitted`, `branch` or `last-turn` scope. Only
// uncommitted changes can be staged, unstaged, reverted (after an
// in-window confirmation) and committed; Commit & Push, branches and
// pull requests run through the session's terminal backend.

import type { GitFile, GitRan, GitReview } from "../../../schema/rpc";
import { h, notice } from "../../../web/src/lib/dom";
import type { DesktopClient } from "./client";
import { confirmIn } from "./confirm";

const SCOPES = ["uncommitted", "branch", "last-turn"] as const;

export class ReviewPane {
  readonly el = h("section", { class: "review", "aria-label": "Review" });
  private readonly scope = h("select", { name: "review-scope", "aria-label": "Review scope" }, ...SCOPES.map((s) => h("option", { value: s }, s)));
  private readonly body = h("div", { class: "review-body" });
  private readonly output = h("pre", { class: "review-output", hidden: true });
  private sessionId: string | null = null;
  /** Each load's number: a slower, older load never replaces a newer one. */
  private loads = 0;

  constructor(
    private readonly client: DesktopClient,
    private readonly dialogs: HTMLElement,
  ) {
    this.scope.addEventListener("change", () => void this.load());
    const refresh = h("button", { type: "button", name: "review-refresh" }, "Refresh");
    refresh.addEventListener("click", () => void this.load());
    this.el.append(h("h3", {}, "Review"), h("div", { class: "review-bar" }, this.scope, refresh), this.body, this.output);
  }

  async follow(sessionId: string | null): Promise<void> {
    this.sessionId = sessionId;
    await this.load();
  }

  async load(): Promise<void> {
    if (!this.sessionId) {
      this.body.replaceChildren(h("p", { class: "muted" }, "Open a chat whose workspace is a git repository."));
      return;
    }
    const load = ++this.loads;
    delete this.el.dataset.ready;
    let shown: () => void;
    try {
      const review = await this.client.call<GitReview>("git.review", { session_id: this.sessionId, scope: this.scope.value });
      shown = () => this.render(review);
    } catch (err) {
      shown = () => this.body.replaceChildren(notice((err as Error).message, "warn"));
    }
    if (load !== this.loads) return;
    shown();
    this.el.dataset.ready = "true";
  }

  private render(review: GitReview): void {
    const branch = h("p", { class: "review-branch" }, `${review.branch ?? "detached HEAD"} · ${review.ahead} ahead · ${review.behind} behind`);
    const files = h("ul", { class: "review-files" }, ...review.files.map((f) => this.file(f, review.read_only)));
    const empty = review.files.length === 0 ? h("p", { class: "muted" }, "No changes in this scope.") : null;
    this.el.dataset.scope = review.scope;
    this.el.dataset.readOnly = String(review.read_only);
    this.body.replaceChildren(branch, files, empty ?? "", ...(review.read_only ? [] : [this.commitForm()]), this.branchForm(), review.gh ? this.prButton() : "");
  }

  private file(f: GitFile, readOnly: boolean): HTMLElement {
    const actions: HTMLElement[] = [];
    if (!readOnly) {
      const stage = h("button", { type: "button", name: f.staged ? "unstage" : "stage" }, f.staged ? "Unstage" : "Stage");
      stage.addEventListener("click", () => void this.change(f.staged ? "git.unstage" : "git.stage", f.path));
      const revert = h("button", { type: "button", name: "revert" }, "Revert");
      revert.addEventListener("click", () => void this.revert(f.path));
      actions.push(stage, revert);
    }
    return h(
      "li",
      { "data-path": f.path, "data-status": f.status, "data-staged": String(f.staged) },
      h("strong", {}, f.path),
      ` ${f.status}${f.staged ? " (staged)" : ""} `,
      ...actions,
      h("details", { class: "diff" }, h("summary", {}, "Diff"), h("pre", { class: "patch" }, f.patch)),
    );
  }

  private async change(method: string, path: string): Promise<void> {
    await this.run(() => this.client.call(method, { session_id: this.sessionId, paths: [path] }));
  }

  /** Revert discards the change: the pane asks first. */
  private async revert(path: string): Promise<void> {
    if (!(await confirmIn(this.dialogs, `Discard every uncommitted change of ${path}? This cannot be undone.`, "Revert"))) return;
    await this.change("git.revert", path);
  }

  private async run(work: () => Promise<unknown>): Promise<void> {
    try {
      const done = (await work()) as Partial<GitRan> | undefined;
      this.show(done?.output ? done.output : "");
    } catch (err) {
      this.show((err as Error).message);
    }
    await this.load();
  }

  private show(text: string): void {
    this.output.hidden = !text;
    this.output.textContent = text;
  }

  private commitForm(): HTMLElement {
    const message = h("textarea", { name: "commit-message", rows: 3, placeholder: "Commit message", "aria-label": "Commit message" });
    const generate = h("button", { type: "button", name: "generate-message" }, "Generate commit message");
    generate.addEventListener("click", async () => {
      try {
        message.value = (await this.client.call<{ message: string }>("git.message", { session_id: this.sessionId })).message;
      } catch (err) {
        this.show((err as Error).message);
      }
    });
    const commit = (push: boolean) => () => void this.run(() => this.client.call("git.commit", { session_id: this.sessionId, message: message.value, push }));
    const button = h("button", { type: "button", name: "commit" }, "Commit");
    button.addEventListener("click", commit(false));
    const pushed = h("button", { type: "button", name: "commit-push" }, "Commit & Push");
    pushed.addEventListener("click", commit(true));
    return h("div", { class: "review-commit" }, message, generate, button, pushed);
  }

  private branchForm(): HTMLElement {
    const name = h("input", { name: "branch-name", placeholder: "branch", "aria-label": "Branch name" });
    const go = (create: boolean) => () => void this.run(() => this.client.call("git.branch", { session_id: this.sessionId, name: name.value.trim(), create }));
    const create = h("button", { type: "button", name: "create-branch" }, "Create branch");
    create.addEventListener("click", go(true));
    const change = h("button", { type: "button", name: "switch-branch" }, "Switch");
    change.addEventListener("click", go(false));
    return h("div", { class: "review-branch-form" }, name, create, change);
  }

  private prButton(): HTMLElement {
    const pr = h("button", { type: "button", name: "create-pr" }, "Create PR");
    pr.addEventListener("click", () => void this.run(() => this.client.call("git.pr", { session_id: this.sessionId })));
    return pr;
  }
}
