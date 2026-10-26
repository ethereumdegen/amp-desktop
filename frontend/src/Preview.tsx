import DOMPurify from "dompurify";
import { useMemo } from "react";
import type { ReactNode } from "react";

const svgTags = [
  "#text", "svg", "g", "defs", "lineargradient", "radialgradient", "stop",
  "rect", "path", "circle", "ellipse", "line", "polyline", "polygon",
  "text", "tspan", "title", "desc", "clippath",
];
const svgAttributes = [
  "xmlns", "viewBox", "width", "height", "x", "y", "x1", "y1", "x2", "y2",
  "cx", "cy", "r", "rx", "ry", "d", "points", "transform", "id",
  "fill", "fill-opacity", "fill-rule", "stroke", "stroke-width", "stroke-opacity",
  "stroke-linecap", "stroke-linejoin", "stroke-dasharray", "opacity", "color",
  "font-family", "font-size", "font-weight", "text-anchor", "dominant-baseline",
  "dx", "dy", "letter-spacing", "clip-path", "clip-rule", "offset",
  "stop-color", "stop-opacity", "gradientunits", "gradienttransform", "spreadmethod",
];

function previewSource(source: string): { uri?: string; error?: string } {
  if (source.length > 256 * 1024)
    return { error: "SVG preview exceeds the 256 KiB limit. The source remains available below." };
  if (!DOMPurify.isSupported)
    return { error: "Safe SVG previews are unavailable in this browser. The source remains available below." };
  const clean = DOMPurify.sanitize(source, {
    NAMESPACE: "http://www.w3.org/2000/svg",
    ALLOWED_TAGS: svgTags,
    ALLOWED_ATTR: svgAttributes,
    ALLOW_DATA_ATTR: false,
    ALLOW_ARIA_ATTR: false,
    KEEP_CONTENT: false,
    RETURN_TRUSTED_TYPE: false,
  });
  const document = new DOMParser().parseFromString(clean, "image/svg+xml");
  const root = document.documentElement;
  if (root.localName !== "svg" || root.namespaceURI !== "http://www.w3.org/2000/svg" ||
      document.querySelector("parsererror"))
    return { error: "This code block is not a valid static SVG drawing." };
  if (root.querySelectorAll("*").length > 3000)
    return { error: "SVG preview contains too many drawing elements." };
  if (!root.querySelector("rect,path,circle,ellipse,line,polyline,polygon,text"))
    return { error: "No static drawing remains after unsafe content is removed." };

  // Sanitized SVG is displayed as an image, never inserted into the app DOM.
  // Presentation references may only target this SVG's own definitions.
  for (const element of [root, ...root.querySelectorAll("*")]) {
    for (const attribute of ["fill", "stroke", "clip-path"]) {
      const value = element.getAttribute(attribute);
      if (value && /url\s*\(/i.test(value) &&
          !/^url\(\s*#[A-Za-z_][\w:.-]*\s*\)$/i.test(value))
        element.removeAttribute(attribute);
    }
  }
  let bounds = root.getAttribute("viewBox")?.trim().split(/[\s,]+/).map(Number);
  if (!bounds) {
    const dimension = (attribute: string) => {
      const value = root.getAttribute(attribute) ?? "";
      return /^\d+(?:\.\d+)?(?:px)?$/.test(value) ? Number.parseFloat(value) : NaN;
    };
    bounds = [0, 0, dimension("width"), dimension("height")];
  }
  if (bounds.length !== 4 || bounds.some((value) => !Number.isFinite(value) || Math.abs(value) > 100000) ||
      bounds[2] <= 0 || bounds[3] <= 0 || bounds[2] > 8192 || bounds[3] > 16384)
    return { error: "SVG previews need a finite, bounded viewBox or width and height." };
  const scale = Math.min(1, 1200 / bounds[2], 2400 / bounds[3]);
  root.setAttribute("viewBox", bounds.join(" "));
  root.setAttribute("width", String(Math.max(1, Math.round(bounds[2] * scale))));
  root.setAttribute("height", String(Math.max(1, Math.round(bounds[3] * scale))));
  if (!root.hasAttribute("color")) root.setAttribute("color", "#e5e5e7");
  return { uri: `data:image/svg+xml;charset=utf-8,${encodeURIComponent(new XMLSerializer().serializeToString(root))}` };
}

export function SvgPreview({ source, children }: { source: string; children: ReactNode }) {
  const preview = useMemo(() => previewSource(source), [source]);
  return (
    <figure className="design-preview">
      <figcaption>Design preview · static SVG mockup, not a captured screenshot</figcaption>
      {preview.uri ? <img className="design-preview-image" src={preview.uri} alt="Generated design mockup" /> :
        <p className="inline-error" role="status">{preview.error}</p>}
      <details className="preview-source">
        <summary>SVG source</summary>
        {children}
      </details>
    </figure>
  );
}
