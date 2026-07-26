// What {{ values.name }} does. Each command is one function, and `run` chooses between them, so
// adding one is adding an entry to the table below.

import { parseArgs } from "node:util";

import { trace } from "@opentelemetry/api";

import type { Config, Flags } from "./platform/index.js";

const tracer = trace.getTracer("{{ values.name }}");

type Command = (config: Config, flags: Flags, argv: string[]) => Promise<number>;

const commands: Record<string, { about: string; run: Command }> = {
  hello: {
    about: "Says hello, and shows what the platform is telling this service",
    run: hello,
  },
  flags: {
    about: "Prints every flag and setting DOC holds for this service",
    run: showFlags,
  },
};

export async function run(config: Config, flags: Flags, argv: string[]): Promise<number> {
  const [name, ...rest] = argv;
  if (name === undefined || name === "-h" || name === "--help") {
    usage();
    return 0;
  }
  const command = commands[name];
  if (command === undefined) {
    console.error(`there is no command called ${name}`);
    usage();
    return 1;
  }
  return tracer.startActiveSpan(name, async (span) => {
    try {
      return await command.run(config, flags, rest);
    } finally {
      span.end();
    }
  });
}

function usage(): void {
  console.log("{{ values.name }} — {{ values.description }}\n\nCommands:");
  for (const [name, command] of Object.entries(commands)) {
    console.log(`  ${name.padEnd(10)} ${command.about}`);
  }
}

async function hello(config: Config, flags: Flags, argv: string[]): Promise<number> {
  const { values } = parseArgs({
    args: argv,
    options: { who: { type: "string", default: "world" } },
    allowPositionals: false,
  });

  // A flag read here is DOC's, with the value this service falls back to beside it.
  let greeting = flags.string("greeting", "Hello");
  if (flags.boolean("shout", false)) greeting = greeting.toUpperCase();
  console.log(`${greeting}, ${values.who} — from ${config.service} in ${config.environment}`);
  return 0;
}

async function showFlags(config: Config, flags: Flags): Promise<number> {
  console.log(`${config.service} reads its flags from ${config.flagsUrl}`);
  console.log(`  greeting = ${JSON.stringify(flags.string("greeting", "Hello"))}`);
  console.log(`  shout    = ${flags.boolean("shout", false)}`);
  if (config.flagsToken === undefined) {
    console.warn("note: DOC_FLAGS_TOKEN is not set, so only public flags are read");
  }
  return 0;
}
