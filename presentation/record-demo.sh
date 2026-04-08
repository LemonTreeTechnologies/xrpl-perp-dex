#!/bin/bash
# Record demo as asciinema cast (terminal recording)
# Output: presentation/demo.cast
#
# Usage: ./presentation/record-demo.sh
# Then convert to gif: agg demo.cast demo.gif
# Or play back: asciinema play demo.cast

DIR="$(dirname "$0")"
CAST="$DIR/demo.cast"

asciinema rec "$CAST" \
    --title "Perp DEX on XRPL — Live Demo" \
    --idle-time-limit 2 \
    --command "bash $DIR/demo-flow.sh"

echo ""
echo "✓ Recorded to $CAST"
echo "  Play: asciinema play $CAST"
echo "  Convert to GIF: agg $CAST demo.gif"
