# Isolated test window for tests/test_desktop.py (never touches the user's apps).
Add-Type -AssemblyName System.Windows.Forms, System.Drawing
[System.Windows.Forms.Application]::EnableVisualStyles()
$f = New-Object System.Windows.Forms.Form
$f.Text = 'nerve-p3-target'
$f.StartPosition = 'Manual'
$f.Location = New-Object System.Drawing.Point(200, 150)
$f.Size = New-Object System.Drawing.Size(900, 600)
$f.TopMost = $true
$font = New-Object System.Drawing.Font('Segoe UI', 16)

$lbl = New-Object System.Windows.Forms.Label
$lbl.Text = 'Привет мир nerve label'
$lbl.Font = $font
$lbl.AutoSize = $true
$lbl.Location = New-Object System.Drawing.Point(20, 15)

$btn = New-Object System.Windows.Forms.Button
$btn.Text = 'Press me'
$btn.Font = $font
$btn.Size = New-Object System.Drawing.Size(200, 50)
$btn.Location = New-Object System.Drawing.Point(20, 60)
$btn.Add_Click({ $lbl.Text = 'clicked ok'; $f.Text = 'nerve-p3-target clicked' })

$tb = New-Object System.Windows.Forms.TextBox
$tb.Multiline = $true
$tb.AcceptsTab = $true
$tb.Font = $font
$tb.AccessibleName = 'Editor'
$tb.Location = New-Object System.Drawing.Point(20, 130)
$tb.Size = New-Object System.Drawing.Size(840, 400)

$f.Controls.AddRange(@($lbl, $btn, $tb))
$f.Add_Shown({ $f.Activate(); $tb.Focus() })
[void]$f.ShowDialog()
