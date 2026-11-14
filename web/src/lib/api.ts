// The dashboard's JSON API. The session cookie is HttpOnly (the SPA never
// sees it); the CSRF token comes from `/api/session` and goes with every
// state-changing call.

export interface SessionInfo {
  subject: string;
  role: "driver" | "observer";
  csrf: string;
  auth_mode: string;
  base_path: string;
  max_upload_bytes: number;
}

/** The base path the dashboard is served under (`""` or `/prometheus`). */
export const base: string =
  document.querySelector<HTMLMetaElement>('meta[name="prometheus-base"]')?.content ?? "";

let csrf = "";

export class ApiError extends Error {
  constructor(
    readonly status: number,
    readonly body: Record<string, unknown>,
  ) {
    super(String(body.message ?? body.error ?? `HTTP ${status}`));
  }
}

async function call(method: string, path: string, body?: unknown, raw?: Blob | File): Promise<unknown> {
  const headers: Record<string, string> = {};
  if (method !== "GET") headers["X-Prometheus-CSRF"] = csrf;
  let payload: BodyInit | undefined;
  if (raw) {
    payload = raw;
  } else if (body !== undefined) {
    headers["Content-Type"] = "application/json";
    payload = JSON.stringify(body);
  }
  const response = await fetch(`${base}${path}`, { method, headers, body: payload, credentials: "same-origin" });
  const text = await response.text();
  let parsed: unknown = text;
  if ((response.headers.get("content-type") ?? "").includes("json")) parsed = text ? JSON.parse(text) : {};
  if (response.status === 401 && path !== "/api/session") document.dispatchEvent(new CustomEvent("signed-out"));
  if (!response.ok) {
    throw new ApiError(response.status, typeof parsed === "object" && parsed ? (parsed as Record<string, unknown>) : { message: text });
  }
  return parsed;
}

export const api = {
  get: <T>(path: string) => call("GET", path) as Promise<T>,
  post: <T>(path: string, body?: unknown) => call("POST", path, body) as Promise<T>,
  put: <T>(path: string, body?: unknown) => call("PUT", path, body) as Promise<T>,
  patch: <T>(path: string, body?: unknown) => call("PATCH", path, body) as Promise<T>,
  del: <T>(path: string) => call("DELETE", path) as Promise<T>,
  upload: <T>(path: string, file: Blob) => call("POST", path, undefined, file) as Promise<T>,
};

/** The signed-in browser, or `null` when it is not signed in. */
export async function signIn(): Promise<SessionInfo | null> {
  try {
    const info = await api.get<SessionInfo>("/api/session");
    csrf = info.csrf;
    return info;
  } catch (err) {
    if (err instanceof ApiError && err.status === 401) return null;
    throw err;
  }
}

export function csrfToken(): string {
  return csrf;
}

/** `/api/profiles/<profile><rest>`: every profile-data call names its profile. */
export function profilePath(profile: string, rest: string): string {
  return `/api/profiles/${encodeURIComponent(profile)}${rest}`;
}
