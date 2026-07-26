// Package command is what {{ values.name }} does. Each command is one function, and `Run` chooses
// between them, so adding one is adding a line to the table below.
package command

import (
	"context"
	"flag"
	"fmt"
	"io"
	"os"
	"strings"

	"go.opentelemetry.io/otel"

	"{{ scaffold.module }}/internal/platform"
)

type command struct {
	name  string
	about string
	run   func(context.Context, platform.Config, *platform.Flags, []string, io.Writer) error
}

var commands = []command{
	{"hello", "Says hello, and shows what the platform is telling this service", hello},
	{"flags", "Prints every flag and setting DOC holds for this service", printFlags},
}

// Run takes the arguments after the program's name and does what they ask for.
func Run(ctx context.Context, config platform.Config, flags *platform.Flags, args []string, out io.Writer) error {
	ctx, span := otel.Tracer(config.Service).Start(ctx, "command")
	defer span.End()

	if len(args) == 0 || args[0] == "-h" || args[0] == "--help" {
		usage(out)
		return nil
	}
	for _, one := range commands {
		if one.name == args[0] {
			return one.run(ctx, config, flags, args[1:], out)
		}
	}
	usage(out)
	return fmt.Errorf("there is no command called %q", args[0])
}

func usage(out io.Writer) {
	fmt.Fprintf(out, "{{ values.name }} — {{ values.description }}\n\nCommands:\n")
	for _, one := range commands {
		fmt.Fprintf(out, "  %-10s %s\n", one.name, one.about)
	}
}

func hello(ctx context.Context, config platform.Config, flags *platform.Flags, args []string, out io.Writer) error {
	set := flag.NewFlagSet("hello", flag.ContinueOnError)
	set.SetOutput(out)
	who := set.String("who", "world", "who to greet")
	if err := set.Parse(args); err != nil {
		return err
	}

	// A flag read here is DOC's, with the value this service falls back to beside it.
	greeting := flags.String("greeting", "Hello")
	if flags.Bool("shout", false) {
		greeting = strings.ToUpper(greeting)
	}
	fmt.Fprintf(out, "%s, %s — from %s in %s\n", greeting, *who, config.Service, config.Environment)
	return nil
}

func printFlags(ctx context.Context, config platform.Config, flags *platform.Flags, args []string, out io.Writer) error {
	fmt.Fprintf(out, "%s reads its flags from %s\n", config.Service, config.FlagsURL)
	fmt.Fprintf(out, "  greeting = %q\n", flags.String("greeting", "Hello"))
	fmt.Fprintf(out, "  shout    = %t\n", flags.Bool("shout", false))
	if config.FlagsToken == "" {
		fmt.Fprintln(os.Stderr, "note: DOC_FLAGS_TOKEN is not set, so only public flags are read")
	}
	return nil
}
