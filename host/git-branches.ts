import { execFile } from "node:child_process";
import { promisify } from "node:util";

const exec = promisify(execFile);
const options = (cwd: string) => ({
  cwd,
  timeout: 10_000,
  maxBuffer: 1024 * 1024,
});

// The desktop recognizes "commit your changes or stash" and offers to stash
// or commit, then retries.
const DIRTY_CHECKOUT =
  "Commit your changes or stash them before switching branches on the host.";

export type HostBranches = {
  current: string | null;
  branches: string[];
  remotes: { remote: string; name: string }[];
};

export async function hostBranches(cwd: string): Promise<HostBranches> {
  const [{ stdout: names }, { stdout: remoteNames }, current] =
    await Promise.all([
      exec(
        "git",
        ["for-each-ref", "--format=%(refname:short)", "refs/heads"],
        options(cwd),
      ),
      exec(
        "git",
        [
          "for-each-ref",
          "--format=%(refname:short)%00%(symref)",
          "refs/remotes",
        ],
        options(cwd),
      ),
      exec("git", ["symbolic-ref", "--quiet", "--short", "HEAD"], options(cwd))
        .then(({ stdout }) => stdout.trim())
        .catch(() => null),
    ]);
  const remotes = remoteNames
    .split("\n")
    .filter(Boolean)
    .flatMap((line) => {
      const [ref, symref] = line.split("\0");
      const slash = ref.indexOf("/");
      return slash > 0 && !symref
        ? [{ remote: ref.slice(0, slash), name: ref.slice(slash + 1) }]
        : [];
    });
  return { current, branches: names.split("\n").filter(Boolean), remotes };
}

export async function switchHostBranch(
  cwd: string,
  branch: unknown,
  remote?: unknown,
): Promise<HostBranches> {
  if (
    typeof branch !== "string" ||
    branch.length > 255 ||
    !branch ||
    branch.startsWith("-")
  )
    throw new Error("Invalid branch");
  const state = await hostBranches(cwd);
  const remoteRef =
    typeof remote === "string" &&
    state.remotes.some(
      (entry) => entry.remote === remote && entry.name === branch,
    )
      ? `refs/remotes/${remote}/${branch}`
      : null;
  if (remote != null && !remoteRef)
    throw new Error("Choose an available remote branch");
  if (!state.branches.includes(branch) && !remoteRef)
    throw new Error("Choose an existing local branch");
  if (state.current === branch) return state;
  const { stdout: changes } = await exec(
    "git",
    ["status", "--porcelain", "--untracked-files=all"],
    options(cwd),
  );
  if (changes)
    throw new Error(DIRTY_CHECKOUT);
  if (remoteRef && !state.branches.includes(branch))
    await exec(
      "git",
      ["switch", "--track", "-c", branch, remoteRef],
      options(cwd),
    );
  else await exec("git", ["switch", branch], options(cwd));
  return hostBranches(cwd);
}

export async function createHostBranch(
  cwd: string,
  branch: unknown,
): Promise<HostBranches> {
  if (
    typeof branch !== "string" ||
    !branch ||
    branch.length > 255 ||
    branch.startsWith("-") ||
    branch.startsWith("@")
  )
    throw new Error("Enter a valid branch name");
  await exec("git", ["check-ref-format", "--branch", branch], options(cwd));
  const state = await hostBranches(cwd);
  if (state.branches.includes(branch)) throw new Error("Branch already exists");
  const { stdout: changes } = await exec(
    "git",
    ["status", "--porcelain", "--untracked-files=all"],
    options(cwd),
  );
  if (changes)
    throw new Error(DIRTY_CHECKOUT);
  await exec("git", ["switch", "-c", branch], options(cwd));
  return hostBranches(cwd);
}

/**
 * The URL of the remote that names this repository: `origin`, then
 * `upstream`, then the first remote by name. Undefined without one.
 */
export async function gitRemoteUrl(cwd: string): Promise<string | undefined> {
  try {
    const { stdout } = await exec("git", ["remote"], options(cwd));
    const names = stdout
      .split("\n")
      .map((name) => name.trim())
      .filter(Boolean);
    const name = ["origin", "upstream"].find((preferred) =>
      names.includes(preferred),
    ) ?? [...names].sort()[0];
    if (!name) return undefined;
    const url = await exec("git", ["remote", "get-url", name], options(cwd));
    return url.stdout.trim() || undefined;
  } catch {
    return undefined;
  }
}

/** A host project with its `remoteUrl`, when it has one. */
export async function withRemoteUrl<T extends { cwd: string }>(
  project: T,
): Promise<T & { remoteUrl?: string }> {
  const remoteUrl = await gitRemoteUrl(project.cwd);
  return remoteUrl ? { ...project, remoteUrl } : project;
}
