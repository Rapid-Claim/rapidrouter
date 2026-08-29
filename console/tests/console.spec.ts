import AxeBuilder from "@axe-core/playwright";
import { expect, test } from "@playwright/test";

async function signIn(page: import("@playwright/test").Page) {
  await page.goto("/console");
  if (await page.getByRole("heading", { name: "Sign in", exact: true }).isVisible()) {
    // Email is the default credential; the admin key lives behind the
    // toggle.
    await page.getByRole("button", { name: "Use admin key" }).click();
    await page.getByLabel("Admin key").fill("admin-e2e-key");
    await page.getByRole("button", { name: "Sign in", exact: true }).click();
  }
  await expect(page.getByRole("heading", { name: "Usage", exact: true, level: 1 })).toBeVisible();
}

test.beforeEach(async ({ page }) => {
  await signIn(page);
});

const PAGES = ["usage", "cost", "activity", "logs", "keys", "providers", "models", "routing", "playground", "settings"] as const;

test("every operator page is reachable from the rail", async ({ page }) => {
  for (const name of [
    "Cost", "Model activity", "Logs", "Virtual keys", "Providers", "Models", "Routing groups", "Playground", "Usage",
  ]) {
    await page.getByRole("link", { name, exact: true }).click();
    await expect(page.getByRole("heading", { name, exact: true, level: 1 })).toBeVisible();
  }
});

test("there is no overview page any more", async ({ page }) => {
  await expect(page.getByRole("link", { name: "Overview", exact: true })).toHaveCount(0);
  // An old bookmark must land somewhere real rather than a blank frame.
  await page.goto("/console#overview");
  await expect(page.getByRole("heading", { name: "Usage", exact: true, level: 1 })).toBeVisible();
});

test("the rail collapses to icons and remembers it", async ({ page, viewport }) => {
  // Below the tablet breakpoint the rail is already a bottom bar of
  // icons, so there is nothing for the toggle to collapse and it is
  // deliberately not rendered.
  test.skip((viewport?.width ?? 0) < 900, "the rail is icon-only at this width already");
  const label = page.getByRole("link", { name: "Providers", exact: true }).locator("span");
  await expect(label).toBeVisible();

  await page.getByRole("button", { name: "Collapse sidebar" }).click();
  // The destination is still reachable — it just has no visible text.
  await expect(label).toBeHidden();
  await expect(page.getByRole("link", { name: "Providers", exact: true })).toBeVisible();

  // A reload must not silently re-expand it.
  await page.reload();
  await expect(page.getByRole("button", { name: "Expand sidebar" })).toBeVisible();
  await expect(page.getByRole("link", { name: "Providers", exact: true }).locator("span")).toBeHidden();

  await page.getByRole("button", { name: "Expand sidebar" }).click();
  await expect(label).toBeVisible();
});

test("popups escape scrolling containers and stay usable", async ({ page }) => {
  // Regression: a popup rendered inside its trigger is clipped by any
  // ancestor that scrolls or hides overflow — the Add provider dialog cut
  // its own provider list in half. They are portalled to the body now.
  await page.getByRole("link", { name: "Providers", exact: true }).click();
  await page.getByRole("button", { name: "Add provider" }).click();
  await page.getByRole("button", { name: "Provider", exact: true }).click();

  const escaped = await page.evaluate(() => {
    const dialog = document.querySelector(".dialog");
    const popup = document.querySelector(".combobox-popup");
    if (!dialog || !popup) return null;
    const d = dialog.getBoundingClientRect();
    const q = popup.getBoundingClientRect();
    return {
      portalled: !dialog.contains(popup),
      withinViewport: q.top >= 0 && q.bottom <= window.innerHeight && q.left >= 0,
      tallerThanTheDialogAllows: q.bottom > d.bottom,
    };
  });
  expect(escaped?.portalled, "the popup must not live inside the dialog").toBe(true);
  expect(escaped?.withinViewport, "and must still be fully on screen").toBe(true);

  // Selecting through the portal still applies.
  await page.getByRole("option", { name: /^Anthropic/ }).click();
  await expect(page.getByRole("button", { name: "Provider", exact: true })).toContainText("Anthropic");
  // One Escape closes the dialog — but only once no popup is open, so a
  // single keypress never collapses two layers at once.
  await page.keyboard.press("Escape");
  await expect(page.getByRole("dialog")).toBeHidden();
});

test("a filter chosen in a nested popup applies without closing the panel", async ({ page }) => {
  await page.getByRole("link", { name: "Logs", exact: true }).click();
  await page.getByRole("button", { name: /Filters/ }).click();
  await page.getByRole("button", { name: "Status" }).click();
  await page.getByRole("option", { name: "Errors only" }).click();
  // The panel hosts the popup but does not contain it in the DOM, so a
  // naive click-away check would dismiss the panel mid-click.
  await expect(page.getByRole("group", { name: "Filters" })).toBeVisible();
  await expect(page.locator(".chip-count")).toHaveText("1");
});

test("settings lives in the nav footer and exports the config", async ({ page }) => {
  await page.getByRole("link", { name: "Settings" }).click();
  await expect(page.getByRole("heading", { name: "Settings", exact: true })).toBeVisible();
  await expect(page.getByRole("heading", { name: "Configuration" })).toBeVisible();
  await expect(page.getByRole("heading", { name: "Usage retention" })).toBeVisible();
  // Cluster state lives here now, not as a page of its own.
  await expect(page.getByRole("heading", { name: "Cluster", exact: true })).toBeVisible();
  await expect(page.getByRole("link", { name: "Cluster", exact: true })).toHaveCount(0);
  const download = page.waitForEvent("download");
  await page.getByRole("button", { name: "Export TOML" }).click();
  expect((await download).suggestedFilename()).toBe("rapid-router.toml");
});

test("a reload connects without ever claiming to be reconnecting", async ({ page }) => {
  // The header used to be a boolean, so a page that had not finished its
  // first handshake rendered "Reconnecting" — which made every reload of
  // a perfectly healthy gateway look like a fault. A first connection is
  // "Connecting", and it settles on "Live".
  const reconnecting: string[] = [];
  await page.exposeFunction("__sawReconnecting", (text: string) => reconnecting.push(text));
  await page.addInitScript(() => {
    // Catch it in the DOM as it happens: polling from the test side
    // would miss a flash that lasts a frame.
    new MutationObserver(() => {
      const label = document.querySelector(".live-label")?.textContent ?? "";
      if (label === "Reconnecting") (window as any).__sawReconnecting?.(label);
    }).observe(document, { childList: true, subtree: true, characterData: true });
  });

  await page.reload();
  await expect(page.locator(".live-label")).toHaveText("Live");
  expect(reconnecting, "a healthy reload must never say Reconnecting").toEqual([]);
});

test("the range picker offers the long windows retention now covers", async ({ page }) => {
  await page.getByRole("button", { name: "Time range" }).click();
  for (const preset of ["Last 90 days", "Last 6 months", "Last year"]) {
    await expect(page.getByRole("button", { name: preset })).toBeVisible();
  }
  await page.getByRole("button", { name: "Last year" }).click();
  await expect(page.getByRole("button", { name: "Time range" })).toContainText("Last year");
  // The whole point of the rollup tiers: a year is not a slow page.
  await expect(page.getByRole("heading", { name: "Usage", exact: true, level: 1 })).toBeVisible();
});

test("keyboard shortcuts jump between pages and focus the filter", async ({ page }) => {
  await page.locator("body").press("g");
  await page.locator("body").press("k");
  await expect(page.getByRole("heading", { name: "Virtual keys", exact: true, level: 1 })).toBeVisible();
  await page.locator("body").press("g");
  await page.locator("body").press("l");
  await expect(page.getByRole("heading", { name: "Logs", exact: true, level: 1 })).toBeVisible();
  await page.locator("body").press("/");
  await expect(page.locator("[data-filter]")).toBeFocused();
  // A shortcut key typed into a field must reach the field, not navigate.
  await page.keyboard.type("g");
  await expect(page.locator("[data-filter]")).toHaveValue("g");
  await expect(page.getByRole("heading", { name: "Logs", exact: true, level: 1 })).toBeVisible();
});

test("creates and reveals a virtual key", async ({ page }) => {
  const name = `browser-${Date.now()}`;
  await page.getByRole("link", { name: "Virtual keys" }).click();
  await page.getByRole("button", { name: "Create key" }).click();
  await page.getByLabel("Name").fill(name);
  // Model scope is a multi-select now; a key with none is "every model",
  // which is what this test wants.
  await page.getByRole("dialog").getByRole("button", { name: "Create key" }).click();
  await expect(page.getByText("Copy this key now")).toBeVisible();
  await expect(page.getByText(/^ck-/)).toBeVisible();
  page.once("dialog", (dialog) => dialog.accept());
  await page.getByRole("button", { name: `Delete ${name}` }).click();
  await expect(page.getByText(name)).not.toBeVisible();
});

async function openServiceAccountsDrawer(page: import("@playwright/test").Page) {
  await page.setViewportSize({ width: 1600, height: 900 });
  await page.route("**/admin/api/keys", (route) => route.fulfill({
    json: {
      data: [{
        id: "vk-agi",
        name: "agi",
        models: [],
        tenant: "agi",
        tags: {},
        enabled: true,
        created_ms: 1,
      }],
    },
  }));
  await page.route("**/admin/api/providers", (route) => route.fulfill({
    json: {
      tenants: ["agi"],
      data: [{
        name: "codex",
        kind: "codex_subscription",
        subscription: true,
        base_url: null,
        managed: true,
        keys: [
          {
            name: "owned",
            weight: 1,
            models: null,
            tenant: "agi",
            health: "healthy",
            status: "ready",
            benched_until_ms: null,
            limits: { rpm: null, tpm: null },
            quota: null,
            credential: {
              email: "owned@example.com",
              account_id: "acct-owned",
              expires_at_ms: null,
              can_refresh: true,
              expired: false,
            },
            last_check: null,
            source_path: null,
          },
          {
            name: "available",
            weight: 1,
            models: null,
            tenant: null,
            health: "healthy",
            status: "ready",
            benched_until_ms: null,
            limits: { rpm: null, tpm: null },
            quota: null,
            credential: {
              email: "available@example.com",
              account_id: "acct-available",
              expires_at_ms: null,
              can_refresh: true,
              expired: false,
            },
            last_check: null,
            source_path: null,
          },
        ],
      }],
    },
  }));

  await page.goto("/console#keys");
  await page.getByRole("button", { name: "1 account", exact: true }).click();
  await expect(page.getByRole("dialog", { name: "Accounts for agi" })).toBeVisible();
}

test("service account rows fill the drawer body", async ({ page }) => {
  await openServiceAccountsDrawer(page);

  const widths = await page.evaluate(() => {
    const body = document.querySelector<HTMLElement>(".drawer-body")!;
    const list = document.querySelector<HTMLElement>(".account-list")!;
    const style = getComputedStyle(body);
    const content = body.clientWidth
      - Number.parseFloat(style.paddingLeft)
      - Number.parseFloat(style.paddingRight);
    return { content, list: list.getBoundingClientRect().width };
  });
  expect(Math.abs(widths.content - widths.list)).toBeLessThanOrEqual(2);
});

test("service account search stays compact on a wide drawer", async ({ page }) => {
  await openServiceAccountsDrawer(page);

  const searchWidth = await page.getByRole("searchbox", { name: "Search this service's accounts" })
    .evaluate((element) => element.getBoundingClientRect().width);
  expect(searchWidth).toBeLessThanOrEqual(448);
});

// WCAG AA is a gate, not a hope: every page, in both themes, including
// the contrast rule the design tokens exist to satisfy.
for (const theme of ["light", "dark"] as const) {
  test(`every page meets WCAG AA in the ${theme} theme`, async ({ page }) => {
    await page.emulateMedia({ colorScheme: theme });
    for (const route of PAGES) {
      await page.goto(`/console#${route}`);
      await expect(page.locator("h1")).toBeVisible();
      // Let resources resolve and colour transitions finish: buttons fade
      // 120ms from their loading state, and axe sampling mid-fade reports
      // blended colours that exist on screen for one frame.
      await page.waitForLoadState("networkidle");
      await page.waitForTimeout(250);
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
    await expect(page.locator("h1")).toBeVisible();
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
