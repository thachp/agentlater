import type { MetadataRoute } from "next";

// /pay is left out: it only works from a payment link.
const PATHS = ["", "/docs", "/docs/capabilities", "/docs/delivery", "/terms", "/privacy"];

export default function sitemap(): MetadataRoute.Sitemap {
  return PATHS.map((path) => ({ url: `https://runlater.io${path}` }));
}
