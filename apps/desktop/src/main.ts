// The desktop app's pages: the main window, the quick-ask panel
// (`#quick-ask`) or the pop-out pet (`#pet`, no connection of its own). Links and windows are guarded before anything renders.
// In a browser (the webapp under `/app`) the workspace starts once the
// browser is signed in.

import { boot } from "./app";
import { guardLinks } from "./links";
import { petOverlay } from "./pet/overlay";
import { quickAsk } from "./quickask";
import { isWebapp } from "./shell";
import { startWebapp } from "./web-shell";

guardLinks((href, reason) => {
  const line = document.createElement("p");
  line.className = "notice warn";
  line.setAttribute("role", "status");
  line.textContent = `Not opened (${reason}): ${href}`;
  document.querySelector(".notices")?.append(line);
});
const root = document.getElementById("app") as HTMLElement;
if (isWebapp()) void startWebapp(root).then((signedIn) => (signedIn ? boot() : undefined));
else if (location.hash === "#quick-ask") void quickAsk(root);
else if (location.hash === "#pet") petOverlay(root);
else void boot();
