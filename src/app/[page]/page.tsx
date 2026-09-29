import type { Metadata } from "next";
import { notFound, redirect } from "next/navigation";
import { findProject } from "@/lib/projects";
import { extensionPage } from "@/lib/extensions";

export const dynamic = "force-dynamic";

/**
 * Pages added by extensions (perso/), at /<slug>. Built-in routes always win over this one.
 * /<project id> also leads to that project's page, so short links keep working.
 */
export async function generateMetadata({ params }: { params: Promise<{ page: string }> }): Promise<Metadata> {
  const page = extensionPage((await params).page);
  return page ? { title: page.title } : {};
}

export default async function ExtensionPage({ params }: { params: Promise<{ page: string }> }) {
  const slug = (await params).page;
  const page = extensionPage(slug);
  if (page) return <page.Page />;
  const p = findProject(slug);
  if (p && p.href !== `/${slug}`) redirect(p.href);
  notFound();
}
