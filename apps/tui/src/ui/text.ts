/** Clip to a width with an ellipsis — rows are one line, always. */
export const clip = (text: string, max: number) =>
  max <= 1 ? "" : text.length > max ? `${text.slice(0, max - 1)}…` : text;
