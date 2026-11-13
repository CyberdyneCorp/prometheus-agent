// The desktop app's pages: the main window, or the quick-ask panel
// (`#quick-ask`). Links and windows are guarded before anything renders.

import { boot } from "./app";
import { guardLinks } from "./links";
import { quickAsk } from "./quickask";

guardLinks((href, reason) => {
  const line = document.createElement("p");
  line.className = "notice warn";
  line.setAttribute("role", "status");
  line.textContent = `Not opened (${reason}): ${href}`;
  document.querySelector(".notices")?.append(line);
});
if (location.hash === "#quick-ask") void quickAsk(document.getElementById("app") as HTMLElement);
else void boot();
