import type { MetadataRoute } from "next";

export default function robots(): MetadataRoute.Robots {
  return {
    rules: { userAgent: "*", allow: "/", disallow: "/pay" },
    sitemap: "https://runlater.io/sitemap.xml",
  };
}
