// The desktop app's page: links and windows are guarded before anything
// renders, then the window attaches to the local daemon.

import { boot } from "./app";
import { guardLinks } from "./links";

guardLinks((href, reason) => {
  const line = document.createElement("p");
  line.className = "notice warn";
  line.setAttribute("role", "status");
  line.textContent = `Not opened (${reason}): ${href}`;
  document.querySelector(".notices")?.append(line);
});
void boot();
