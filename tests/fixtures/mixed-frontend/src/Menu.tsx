declare function clsx(...names: unknown[]): string;
export function Menu(props: { big: boolean }) {
  return <nav className={clsx("btn", { "btn-lg": props.big })}>Menu</nav>;
}
