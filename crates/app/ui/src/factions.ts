/// Faction display data shared across views.
/// Colors follow the in-game faction palette: Terran blue, Zerg purple,
/// Protoss gold, Nova ghost-teal.

export const FACTION_TITLES: Record<string, string> = {
  wol: "WoL",
  hots: "HotS",
  lotv: "LotV",
  nco: "NCO",
};

export const FACTION_NAMES: Record<string, string> = {
  wol: "Wings of Liberty",
  hots: "Heart of the Swarm",
  lotv: "Legacy of the Void",
  nco: "Nova Covert Ops",
};

/** Mantine color names matching each faction's in-game identity. */
export const FACTION_COLORS: Record<string, string> = {
  wol: "blue",
  hots: "grape",
  lotv: "yellow",
  nco: "cyan",
};
