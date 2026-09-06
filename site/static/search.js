// Client-side search over Zola's generated index.
//
// Deliberately small: the index is fetched only when someone types, so a reader who never searches
// downloads nothing. With JavaScript off the input is inert and the sidebar still lists every page.

(() => {
  const input = document.getElementById("q");
  const results = document.getElementById("results");
  if (!input || !results) return;

  const base = document.querySelector('link[rel="sitemap"]').href.replace(/sitemap\.xml$/, "");
  let index = null;
  let loading = null;
  let selected = -1;

  async function load() {
    if (index) return index;
    if (!loading) {
      loading = fetch(base + "search_index.en.json")
        .then((r) => r.json())
        .then((data) => {
          // The index stores each document under its permalink; flatten it to what we render.
          index = Object.entries(data.documentStore.docs).map(([id, doc]) => ({
            id,
            title: doc.title || "",
            body: (doc.body || "").toLowerCase(),
            titleLower: (doc.title || "").toLowerCase(),
          }));
          return index;
        })
        .catch(() => (index = []));
    }
    return loading;
  }

  function close() {
    results.hidden = true;
    input.setAttribute("aria-expanded", "false");
    input.removeAttribute("aria-activedescendant");
    selected = -1;
  }

  function highlight(next) {
    const items = [...results.querySelectorAll("li[role='option']")];
    if (!items.length) return;
    // Wraps at both ends, so ↓ from the last result returns to the first rather than sticking.
    selected = (next + items.length) % items.length;
    items.forEach((li, i) => {
      const on = i === selected;
      li.setAttribute("aria-selected", on ? "true" : "false");
      if (on) {
        input.setAttribute("aria-activedescendant", li.id);
        li.scrollIntoView({ block: "nearest" });
      }
    });
  }

  function render(matches, query) {
    results.innerHTML = "";
    selected = -1;
    input.removeAttribute("aria-activedescendant");

    if (!query) {
      close();
      return;
    }
    if (!matches.length) {
      // Saying so beats an empty box that looks like a page that has not loaded.
      const li = document.createElement("li");
      li.className = "hint";
      li.textContent = `Nothing matches “${query}”`;
      results.appendChild(li);
      results.hidden = false;
      input.setAttribute("aria-expanded", "true");
      return;
    }
    matches.slice(0, 8).forEach((m, i) => {
      const li = document.createElement("li");
      li.id = `result-${i}`;
      li.setAttribute("role", "option");
      li.setAttribute("aria-selected", "false");
      const a = document.createElement("a");
      a.href = m.id;
      a.textContent = m.title;
      li.appendChild(a);
      results.appendChild(li);
    });
    results.hidden = false;
    input.setAttribute("aria-expanded", "true");
  }

  let timer;
  input.addEventListener("input", () => {
    clearTimeout(timer);
    timer = setTimeout(async () => {
      const q = input.value.trim().toLowerCase();
      if (q.length < 2) {
        render([], "");
        return;
      }
      const docs = await load();
      // A title hit outranks any body hit; among body hits, the page that says the word most
      // often wins. Good enough for a couple of dozen pages, and it needs no ranking library.
      const scored = docs
        .map((d) => {
          const hits = d.body.split(q).length - 1;
          return { doc: d, score: d.titleLower.includes(q) ? 1000 + hits : hits };
        })
        .filter((s) => s.score > 0)
        .sort((a, b) => b.score - a.score)
        .map((s) => s.doc);
      render(scored, input.value.trim());
    }, 120);
  });

  input.addEventListener("keydown", (e) => {
    if (e.key === "ArrowDown") {
      e.preventDefault();
      highlight(selected + 1);
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      highlight(selected - 1);
    } else if (e.key === "Enter") {
      const active = results.querySelector("li[aria-selected='true'] a");
      if (active) {
        e.preventDefault();
        active.click();
      }
    } else if (e.key === "Escape") {
      // First press dismisses the list, a second clears the box — the order every search field
      // in a browser uses.
      if (!results.hidden) close();
      else input.value = "";
    }
  });

  // `/` focuses search, the convention every documentation site of the last decade shares. Ignored
  // while the reader is typing somewhere else, which is what stops it eating a slash in a URL.
  document.addEventListener("keydown", (e) => {
    if (e.key !== "/" || e.metaKey || e.ctrlKey || e.altKey) return;
    const t = e.target;
    if (t instanceof HTMLElement && (t.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(t.tagName))) return;
    e.preventDefault();
    input.focus();
    input.select();
  });

  // A click inside the list is a navigation, so the blur that precedes it must not hide the link
  // before the click lands.
  input.addEventListener("blur", () => setTimeout(close, 150));
  input.addEventListener("focus", () => {
    if (results.children.length) {
      results.hidden = false;
      input.setAttribute("aria-expanded", "true");
    }
  });
})();
