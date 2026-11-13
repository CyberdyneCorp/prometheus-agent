// The dashboard SPA: sign-in check, the shell (navigation, profile
// selector) and the route table of the built-in pages (spec: web-dashboard,
// Built-in pages).

import { api, signIn } from "./lib/api";
import { onSignedOut } from "./lib/events";
import { h } from "./lib/dom";
import { href, notFound, route, start } from "./lib/router";
import { state } from "./lib/state";
import { loadPlugins, placeTabs } from "./lib/plugins";
import type { Contribution } from "./lib/plugin-frames";
import { chatPage } from "./pages/chat";
import { adaptersPage } from "./pages/adapters";
import { docsPage } from "./pages/docs";
import { filesPage } from "./pages/files";
import { profileBuilderPage } from "./pages/profile-builder";
import { applyTheme, currentLayout, currentTheme, themesPage } from "./pages/themes";
import { pluginsPage } from "./pages/plugins";
import { webhooksPage } from "./pages/webhooks";
import {
  cronPage,
  mcpPage,
  memoryPage,
  modelsPage,
  profilesPage,
  skillsPage,
  toolsetsPage,
} from "./pages/catalog";
import { configPage } from "./pages/config";
import { logsPage } from "./pages/logs";
import { notFoundPage } from "./pages/notfound";
import { evolutionPage, experiencePage, pendingPage } from "./pages/rpcview";
import { secretsPage } from "./pages/secrets";
import { sessionsPage } from "./pages/sessions";
import { statusPage } from "./pages/status";
import { usagePage } from "./pages/usage";
import { workspacePage } from "./pages/workspace";

const NAV: [string, string][] = [
  ["/chat", "Chat"],
  ["/sessions", "Sessions"],
  ["/usage", "Usage"],
  ["/models", "Models"],
  ["/skills", "Skills"],
  ["/plugins", "Plugins"],
  ["/mcp", "MCP"],
  ["/cron", "Cron"],
  ["/adapters", "Adapters"],
  ["/pairing", "Pairing"],
  ["/webhooks", "Webhooks"],
  ["/memory", "Memory"],
  ["/logs", "Logs"],
  ["/status", "Status"],
  ["/profiles", "Profiles"],
  ["/config", "Config"],
  ["/secrets", "Secrets"],
  ["/kanban", "Kanban"],
  ["/evolution", "Evolution"],
  ["/experience", "Experience"],
  ["/files", "Files"],
  ["/docs", "Docs"],
  ["/profile-builder", "Profile builder"],
  ["/themes", "Themes"],
  ["/achievements", "Achievements"],
];

/** Pages whose backends arrive with later capabilities. */
const PENDING: [string, string, string][] = [
  ["/pairing", "Pairing", "Pairing approval in the dashboard"],
  ["/kanban", "Kanban", "The kanban board"],
  ["/achievements", "Achievements", "Achievements"],
];

function routes(): void {
  route("/chat", chatPage);
  route("/sessions", sessionsPage);
  route("/usage", usagePage);
  route("/models", modelsPage);
  route("/skills", skillsPage);
  route("/plugins", pluginsPage);
  route("/mcp", mcpPage);
  route("/cron", cronPage);
  route("/memory", memoryPage);
  route("/toolsets", toolsetsPage);
  route("/adapters", adaptersPage);
  route("/logs", logsPage);
  route("/status", statusPage);
  route("/profiles", profilesPage);
  route("/config", configPage);
  route("/secrets", secretsPage);
  route("/app", workspacePage);
  route("/webhooks", webhooksPage);
  route("/files", filesPage);
  route("/docs", docsPage);
  route("/profile-builder", profileBuilderPage);
  route("/themes", themesPage);
  route("/evolution", evolutionPage);
  route("/experience", experiencePage);
  for (const [path, title, what] of PENDING) route(path, pendingPage(title, what));
  notFound(notFoundPage);
}

function header(): HTMLElement {
  const select = h("select", { "aria-label": "Profile", name: "profile" }, ...state.profiles.map((p) => h("option", { value: p, selected: p === state.profile }, p)));
  select.addEventListener("change", () => {
    state.profile = select.value;
    const url = new URL(location.href);
    url.searchParams.set("profile", select.value);
    url.searchParams.delete("session");
    location.assign(url.toString());
  });
  const signOut = h("button", { type: "button" }, "Sign out");
  signOut.addEventListener("click", async () => {
    await api.post("/api/logout");
    location.assign(href("/"));
  });
  return h(
    "header",
    { class: "top" },
    slot("header-left"),
    h("a", { class: "brand", href: href("/sessions") }, "Prometheus"),
    select,
    h("span", { class: "role", "data-role": state.session?.role ?? "" }, state.session?.role ?? ""),
    signOut,
    slot("header-right"),
  );
}

/** A shell slot plugins may fill. */
function slot(name: string): HTMLElement {
  return h("div", { class: `shell-slot slot-${name}`, "data-shell-slot": name });
}

function navList(plugins: Contribution[]): HTMLElement {
  return h("ul", {}, ...placeTabs(NAV, plugins).map(([path, label]) => h("li", {}, h("a", { href: href(path) }, label))));
}

function nav(): HTMLElement {
  return h("nav", { "aria-label": "Pages" }, navList([]));
}

function signedOut(app: HTMLElement): void {
  app.replaceChildren(
    h(
      "main",
      { class: "signed-out" },
      h("h1", {}, "Sign in to Prometheus"),
      h("p", {}, "Run prometheus dashboard (or prometheus webapp) on this machine to get a one-time login link."),
    ),
  );
}

async function boot(): Promise<void> {
  applyTheme(currentTheme(), currentLayout());
  const app = document.getElementById("app") as HTMLElement;
  const session = await signIn();
  if (!session) {
    signedOut(app);
    return;
  }
  state.session = session;
  const ended = () => signedOut(app);
  onSignedOut(ended);
  document.addEventListener("signed-out", ended);
  state.profiles = (await api.get<{ profiles: { name: string }[] }>("/api/profiles")).profiles.map((p) => p.name);
  const asked = new URLSearchParams(location.search).get("profile");
  state.profile = asked && state.profiles.includes(asked) ? asked : (state.profiles[0] ?? "");
  const main = h("main", { id: "main" });
  const pages = nav();
  app.replaceChildren(
    header(),
    slot("header-banner"),
    h("div", { class: "layout" }, pages, h("div", { class: "main-column" }, slot("pre-main"), main, slot("post-main")), slot("sidebar")),
    h("footer", { class: "bottom" }, slot("footer-left"), slot("footer-right")),
    slot("overlay"),
  );
  routes();
  start(main);
  // Plugins load after the first render and never hold it up.
  void loadPlugins((plugins) => pages.replaceChildren(navList(plugins)));
}

void boot();
