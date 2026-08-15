import AxeBuilder from "@axe-core/playwright";
import { expect, test } from "@playwright/test";

async function signIn(page: import("@playwright/test").Page) {
  await page.goto("/console");
  if (await page.getByRole("heading", { name: "Operator sign in" }).isVisible()) {
    await page.getByLabel("Admin key").fill("admin-e2e-key");
    await page.getByRole("button", { name: "Sign in" }).click();
  }
  await expect(page.getByRole("heading", { name: "Overview" })).toBeVisible();
}

test.beforeEach(async ({ page }) => {
  await signIn(page);
});

const PAGES = ["overview", "providers", "routing", "keys", "usage", "requests", "playground", "fleet", "settings"] as const;

test("all eight operator pages are reachable", async ({ page }) => {
  for (const name of ["Providers", "Routing", "Keys", "Usage", "Requests", "Playground", "Fleet", "Overview"]) {
    await page.getByRole("link", { name, exact: true }).click();
    await expect(page.getByRole("heading", { name, exact: true })).toBeVisible();
  }
});

test("settings lives in the nav footer and exports the config", async ({ page }) => {
  await page.getByRole("link", { name: "Settings" }).click();
  await expect(page.getByRole("heading", { name: "Settings", exact: true })).toBeVisible();
  await expect(page.getByRole("heading", { name: "Configuration" })).toBeVisible();
  await expect(page.getByRole("heading", { name: "Usage retention" })).toBeVisible();
  const download = page.waitForEvent("download");
  await page.getByRole("button", { name: "Export TOML" }).click();
  expect((await download).suggestedFilename()).toBe("caret-router.toml");
});

test("keyboard shortcuts jump between pages and focus the filter", async ({ page }) => {
  await page.locator("body").press("g");
  await page.locator("body").press("k");
  await expect(page.getByRole("heading", { name: "Keys", exact: true })).toBeVisible();
  await page.locator("body").press("g");
  await page.locator("body").press("q");
  await expect(page.getByRole("heading", { name: "Requests", exact: true })).toBeVisible();
  await page.locator("body").press("/");
  await expect(page.locator("[data-filter]")).toBeFocused();
  // A shortcut key typed into a field must reach the field, not navigate.
  await page.keyboard.type("g");
  await expect(page.locator("[data-filter]")).toHaveValue("g");
  await expect(page.getByRole("heading", { name: "Requests", exact: true })).toBeVisible();
});

test("creates and reveals a virtual key", async ({ page }) => {
  const name = `browser-${Date.now()}`;
  await page.getByRole("link", { name: "Keys" }).click();
  await page.getByRole("button", { name: "Create key" }).click();
  await page.getByLabel("Name").fill(name);
  await page.getByLabel("Allowed models Optional").fill("ollama/llama3");
  await page.getByRole("dialog").getByRole("button", { name: "Create key" }).click();
  await expect(page.getByText("Copy this key now")).toBeVisible();
  await expect(page.getByText(/^ck-/)).toBeVisible();
  page.once("dialog", (dialog) => dialog.accept());
  await page.getByRole("button", { name: `Delete ${name}` }).click();
  await expect(page.getByText(name)).not.toBeVisible();
});

// WCAG AA is a gate, not a hope: every page, in both themes, including
// the contrast rule the design tokens exist to satisfy.
for (const theme of ["light", "dark"] as const) {
  test(`every page meets WCAG AA in the ${theme} theme`, async ({ page }) => {
    await page.emulateMedia({ colorScheme: theme });
    for (const route of PAGES) {
      await page.goto(`/console#${route}`);
      await expect(page.locator("main h1")).toBeVisible();
      const results = await new AxeBuilder({ page })
        .withTags(["wcag2a", "wcag2aa", "wcag21a", "wcag21aa"])
        .analyze();
      const serious = results.violations.filter((violation) =>
        ["serious", "critical"].includes(violation.impact ?? ""),
      );
      expect(
        serious,
        `${route} (${theme}): ${serious.map((v) => `${v.id} @ ${v.nodes[0]?.target}`).join(", ")}`,
      ).toEqual([]);
    }
  });
}

test("layout does not overflow the viewport", async ({ page }) => {
  for (const route of PAGES) {
    await page.goto(`/console#${route}`);
    await expect(page.locator("main h1")).toBeVisible();
    const overflow = await page.evaluate(() => ({
      overflowing: document.documentElement.scrollWidth > document.documentElement.clientWidth,
      width: document.documentElement.scrollWidth,
      viewport: document.documentElement.clientWidth,
      widest: [...document.querySelectorAll<HTMLElement>("body *")]
        .map((element) => ({ tag: element.tagName, className: element.className, right: element.getBoundingClientRect().right, width: element.scrollWidth }))
        .sort((a, b) => b.right - a.right)
        .slice(0, 3),
    }));
    expect(overflow.overflowing, `${route}: ${JSON.stringify(overflow)}`).toBe(false);
  }
});
