package main

import (
	"fmt"
	"os"

	//_"charm.land/bubbles/v2/spinner"
	"charm.land/bubbles/v2/textinput"
	tea "charm.land/bubbletea/v2"
	//_"charm.land/lipgloss/v2"
)

type model struct {
	name  string
	input textinput.Model
}

func (m model) Init() tea.Cmd {
	return nil
}

func (m model) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	switch msg := msg.(type) {
	case tea.KeyPressMsg:
		switch msg.String() {
		case "q", "esc", "ctrl+c":
			return m, tea.Quit
		}
	}

	var cmd tea.Cmd
	m.input, cmd = m.input.Update(msg)
	return m, cmd
}

func (m model) View() tea.View {
	//return tea.NewView(fmt.Sprintf("Hello, %s!\n\nPress q to quit.\n", m.name))
	return tea.NewView(m.input.View())
}

func main() {
	fmt.Println("Ingress")
	p := tea.NewProgram(model{name: "World"})
	if _, err := p.Run(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}
