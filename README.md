# baho

**Find the data you need in a file with a short, plain-language command.**

baho is a desktop app for exploring tables and turning them into useful results. Its name means “to flow” in Nepali and Hindi. Open a CSV, browse the detected table, and describe what you want to see. Baho shows the result in a grid while keeping the original file intact.

## Get baho

Desktop binaries for multiple operating systems will be published on [GitHub Releases](https://github.com/brainless/baho/releases) soon. CSV is the first supported file format. Excel and PDF support are planned.

## Use the desktop app

1. Open baho and drop in a CSV file, or open the app with a CSV file path.
2. Browse the table in the grid. Scroll through rows and columns to inspect the source data.
3. Type a short command in the sidebar and select **Submit**. The grid shows the matching result.

Each command starts from the original table, so you can try another request without changing your file or building on the previous result. Baho keeps a list of commands for the open file during that app session. If a request is unclear or unsupported, it reports the issue instead of guessing.

### Example commands

Use the column names in your own file. For a table with columns such as `Floor Plan`, `Income`, `Job`, `Annual Income`, and `Status`, try:

| Command | Result |
| --- | --- |
| `List income` | Show the `Income` column. |
| `Extract all the unique floor plans` | Show distinct, nonblank values from `Floor Plan`. |
| `List rows where Job = unemployed` | Show rows whose `Job` value is `unemployed`. |
| `List rows where Job = unemployed or Annual Income < 10000` | Show rows matching either condition. |
| `List rows where Status = inactive` | Show matching rows regardless of letter case in the status value. |

Commands follow a small, predictable set of patterns. Naming a column explicitly is the clearest way to filter it. If a value could refer to more than one column, baho asks for a more specific request.

## Use the CLI for automation

The same CSV operations are available from a command line for scripts and repeatable runs:

```text
baho run orders.csv --prompt "List rows where Status = inactive"
```

Each run records its result and diagnostic details locally, so you can inspect what happened.

## What’s next

Baho is growing toward workflows you can save and apply to other files with the same structure. That will make recurring work—such as extracting financial figures, finding orders, and running calculations—easier to repeat. Spreadsheet and PDF support are also planned. These workflow features and file formats are not part of the initial CSV release.
